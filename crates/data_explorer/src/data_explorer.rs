//! The data viewer: `View(df)` opens a tab showing the data frame as a grid.
//! Rows come from R a block at a time as they scroll into view, and sorting
//! happens in R, so even a 10M-row data frame stays responsive.

mod model;

use anyhow::{Context as _, Result};
use gpui::{
    AbsoluteLength, App, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Subscription,
    Task, Window, px,
};
use model::{Block, ExplorerData, PAGE_COLUMNS};
use runtime::comms::{
    ArraySelection, BackendState, ColumnProfileRequest, ColumnProfileSpec, ColumnProfileType,
    ColumnSelection, ColumnSortKey, DataExplorerEvent, DataSelectionRange, FormatOptions,
    GetColumnProfilesParams, GetDataValuesParams, GetRowLabelsParams, GetSchemaParams, TableData,
    TableRowLabels, TableSchema,
};
use runtime::{RSession, SessionAdded, SessionEvent, SessionRegistry};
use std::ops::Range;
use std::time::Duration;
use ui::{
    ColumnWidthConfig, IconButton, IconName, ResizableColumnsState, Table, TableInteractionState,
    TableResizeBehavior, Tooltip, prelude::*,
};
use util::ResultExt as _;
use workspace::Workspace;
use workspace::item::Item;

/// Ark answers on R's thread; sorting 10M rows can take a few seconds.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const COLUMN_WIDTH: f32 = 120.;
/// Ark answers one request at a time, so dragging the scrollbar would queue a
/// fetch for every block passed. Capping them lets the rows where the drag
/// stops load next.
const MAX_FETCHES_IN_FLIGHT: usize = 4;

/// Opens a viewer tab whenever Ark opens a data explorer for this window's R.
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let workspace_id = cx.entity_id();
        if let Some(registry) = SessionRegistry::global(cx) {
            if let Some(session) = registry.read(cx).session(workspace_id).cloned() {
                watch_session(workspace, &session, window, cx);
            }
            cx.subscribe_in(
                &registry,
                window,
                move |workspace, _, event: &SessionAdded, window, cx| {
                    if event.workspace_id == workspace_id {
                        watch_session(workspace, &event.session, window, cx);
                    }
                },
            )
            .detach();
        }
    })
    .detach();
}

fn watch_session(
    _workspace: &mut Workspace,
    session: &Entity<RSession>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    cx.subscribe_in(session, window, |workspace, session, event, window, cx| {
        if let SessionEvent::DataExplorerOpened { comm_id, title } = event {
            open_explorer(
                workspace,
                session.clone(),
                comm_id.clone(),
                title.clone(),
                window,
                cx,
            );
        }
    })
    .detach();
}

/// Shows a new data explorer, reusing the tab of the same data frame if one is
/// open, since `View()` is often run again on the same object.
pub fn open_explorer(
    workspace: &mut Workspace,
    session: Entity<RSession>,
    comm_id: String,
    title: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace
        .items_of_type::<DataExplorerView>(cx)
        .find(|view| view.read(cx).title.as_ref() == title);
    if let Some(view) = existing {
        view.update(cx, |view, cx| view.rebind(comm_id, cx));
        workspace.activate_item(&view, true, true, window, cx);
        return;
    }
    let view = cx.new(|cx| DataExplorerView::new(session, comm_id, title.into(), window, cx));
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

#[derive(Clone, Debug, PartialEq)]
enum Status {
    Loading,
    Ready,
    /// The comm is gone, so nothing more can be fetched.
    Closed,
}

pub struct DataExplorerView {
    session: Entity<RSession>,
    comm_id: String,
    title: SharedString,
    data: ExplorerData,
    status: Status,
    /// The last failed request, shown in the toolbar.
    error: Option<String>,
    /// Bumped whenever fetched data becomes stale, so late replies are dropped.
    generation: usize,
    profile_callback: Option<String>,
    fetches_in_flight: usize,
    focus_handle: FocusHandle,
    interaction: Entity<TableInteractionState>,
    widths: Entity<ResizableColumnsState>,
    _subscription: Subscription,
}

impl DataExplorerView {
    pub fn new(
        session: Entity<RSession>,
        comm_id: String,
        title: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.subscribe_in(&session, window, |this, _, event, _, cx| {
            this.handle_session_event(event, cx)
        });
        let mut this = Self {
            session,
            comm_id,
            title,
            data: ExplorerData::default(),
            status: Status::Loading,
            error: None,
            generation: 0,
            profile_callback: None,
            fetches_in_flight: 0,
            focus_handle: cx.focus_handle(),
            interaction: cx.new(|cx| TableInteractionState::new(cx)),
            widths: cx.new(|_| column_widths(0)),
            _subscription: subscription,
        };
        this.reload(true, cx);
        this
    }

    /// Points the tab at a new explorer comm for the same data frame.
    fn rebind(&mut self, comm_id: String, cx: &mut Context<Self>) {
        let old = std::mem::replace(&mut self.comm_id, comm_id);
        if self.status != Status::Closed {
            self.session
                .update(cx, |session, _| session.close_comm(&old))
                .log_err();
        }
        self.status = Status::Loading;
        self.data = ExplorerData::default();
        self.reload(true, cx);
    }

    fn handle_session_event(&mut self, event: &SessionEvent, cx: &mut Context<Self>) {
        match event {
            SessionEvent::DataExplorerEvent { comm_id, event } if *comm_id == self.comm_id => {
                match event {
                    DataExplorerEvent::DataUpdate => self.reload(false, cx),
                    DataExplorerEvent::SchemaUpdate => self.reload(true, cx),
                    DataExplorerEvent::ReturnColumnProfiles(profiles) => {
                        if self.profile_callback.as_deref() != Some(profiles.callback_id.as_str()) {
                            return;
                        }
                        let columns = self.data.page_columns();
                        for (column, profile) in columns.zip(&profiles.profiles) {
                            if let Some(count) = profile.null_count {
                                self.data.null_counts.insert(column as i64, count);
                            }
                        }
                        cx.notify();
                    }
                }
            }
            SessionEvent::DataExplorerClosed { comm_id } if *comm_id == self.comm_id => {
                self.status = Status::Closed;
                self.generation += 1;
                cx.notify();
            }
            _ => {}
        }
    }

    /// Fetches the shape and columns again, dropping fetched rows. With
    /// `schema`, the old columns are hidden meanwhile, as they may be gone.
    /// Rows aren't fetched until the shape arrives, since it may have changed.
    fn reload(&mut self, schema: bool, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.data.invalidate_rows();
        if schema {
            self.data.columns.clear();
        }
        if self.status != Status::Closed {
            self.status = Status::Loading;
        }
        let state = self.request("get_state", serde_json::Value::Null, cx);
        cx.spawn(async move |this, cx| {
            let state: Result<BackendState> = async {
                let state = state?.await?;
                serde_json::from_value(state).context("unexpected reply to get_state")
            }
            .await;
            let Some(schema_request) = this
                .update(cx, |this, cx| {
                    if this.generation != generation {
                        return None;
                    }
                    let state = match state {
                        Ok(state) => state,
                        Err(error) => {
                            this.fail(error, cx);
                            return None;
                        }
                    };
                    this.data.shape = Some(state.table_shape);
                    this.data.has_row_labels = state.has_row_labels;
                    this.data.sort = state.sort_keys.first().copied();
                    if this.data.page_columns().is_empty() {
                        this.data.first_column = 0;
                    }
                    let column_indices: Vec<i64> = this
                        .data
                        .page_columns()
                        .map(|column| column as i64)
                        .collect();
                    let request = this.request(
                        "get_schema",
                        serde_json::to_value(GetSchemaParams { column_indices }).ok()?,
                        cx,
                    );
                    Some(request)
                })
                .ok()
                .flatten()
            else {
                return;
            };
            let schema: Result<TableSchema> = async {
                let schema = schema_request?.await?;
                serde_json::from_value(schema).context("unexpected reply to get_schema")
            }
            .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match schema {
                    Ok(schema) => {
                        let columns = schema.columns.len();
                        this.data.columns = schema.columns;
                        this.status = Status::Ready;
                        this.error = None;
                        if this.widths.read(cx).cols() != columns + 1 {
                            this.widths = cx.new(|_| column_widths(columns));
                        }
                        this.request_null_counts(cx);
                    }
                    Err(error) => this.fail(error, cx),
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    /// Ark counts missing values asynchronously and answers with an event.
    fn request_null_counts(&mut self, cx: &mut Context<Self>) {
        let callback_id = uuid::Uuid::new_v4().to_string();
        let profiles = self
            .data
            .page_columns()
            .map(|column| ColumnProfileRequest {
                column_index: column as i64,
                profiles: vec![ColumnProfileSpec {
                    profile_type: ColumnProfileType::NullCount,
                    params: None,
                }],
            })
            .collect();
        let Ok(params) = serde_json::to_value(GetColumnProfilesParams {
            callback_id: callback_id.clone(),
            profiles,
            format_options: FormatOptions::default(),
        }) else {
            return;
        };
        self.profile_callback = Some(callback_id);
        if let Ok(request) = self.request("get_column_profiles", params, cx) {
            cx.spawn(async move |this, cx| {
                if let Err(error) = request.await {
                    this.update(cx, |this, cx| this.fail(error, cx)).log_err();
                }
            })
            .detach();
        }
    }

    /// Requests the blocks covering `rows` that aren't fetched yet.
    fn ensure_rows(&mut self, rows: Range<usize>, cx: &mut Context<Self>) {
        if self.status != Status::Ready {
            return;
        }
        let limit = MAX_FETCHES_IN_FLIGHT.saturating_sub(self.fetches_in_flight);
        for block in self.data.claim_missing_blocks(rows, limit) {
            self.fetch_block(block, cx);
        }
    }

    fn fetch_block(&mut self, block: usize, cx: &mut Context<Self>) {
        let generation = self.generation;
        let rows = self.data.block_rows(block);
        let Some(last) = rows.end.checked_sub(1) else {
            return;
        };
        let range = ArraySelection::SelectRange(DataSelectionRange {
            first_index: rows.start as i64,
            last_index: last as i64,
        });
        let values = serde_json::to_value(GetDataValuesParams {
            columns: self
                .data
                .page_columns()
                .map(|column| ColumnSelection {
                    column_index: column as i64,
                    spec: range.clone(),
                })
                .collect(),
            format_options: FormatOptions::default(),
        });
        let values = values
            .map_err(anyhow::Error::from)
            .and_then(|params| self.request("get_data_values", params, cx));
        let labels = self.data.has_row_labels.then(|| {
            serde_json::to_value(GetRowLabelsParams {
                selection: range.clone(),
                format_options: FormatOptions::default(),
            })
            .map_err(anyhow::Error::from)
            .and_then(|params| self.request("get_row_labels", params, cx))
        });
        self.fetches_in_flight += 1;
        cx.spawn(async move |this, cx| {
            let block_value = async {
                let values: TableData = serde_json::from_value(values?.await?)
                    .context("unexpected reply to get_data_values")?;
                let labels = match labels {
                    Some(labels) => {
                        let labels: TableRowLabels = serde_json::from_value(labels?.await?)
                            .context("unexpected reply to get_row_labels")?;
                        labels.row_labels.into_iter().next()
                    }
                    None => None,
                };
                anyhow::Ok(Block::Loaded {
                    columns: values.columns,
                    labels,
                })
            }
            .await
            .unwrap_or_else(|error| Block::Failed(format!("{error:#}")));
            this.update(cx, |this, cx| {
                this.fetches_in_flight = this.fetches_in_flight.saturating_sub(1);
                // Rendering again lets rows that waited for a free fetch ask now.
                cx.notify();
                if this.generation != generation {
                    return;
                }
                if let Block::Failed(message) = &block_value {
                    this.error = Some(message.clone());
                }
                this.data.set_block(block, block_value);
            })
            .log_err();
        })
        .detach();
    }

    fn sort(&mut self, column_index: i64, cx: &mut Context<Self>) {
        if self.status != Status::Ready {
            return;
        }
        let key = self.data.next_sort(column_index);
        let params =
            serde_json::json!({ "sort_keys": key.into_iter().collect::<Vec<ColumnSortKey>>() });
        let request = self.request("set_sort_columns", params, cx);
        self.data.sort = key;
        self.generation += 1;
        self.data.invalidate_rows();
        // Rows fetched before R has sorted would come back in the old order.
        self.status = Status::Loading;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = match request {
                Ok(request) => request.await,
                Err(error) => Err(error),
            };
            this.update(cx, |this, cx| match result {
                Ok(_) => this.reload(false, cx),
                Err(error) => this.fail(error, cx),
            })
            .log_err();
        })
        .detach();
    }

    fn page_columns(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.data.page(forward) {
            self.reload(true, cx);
        }
    }

    fn fail(&mut self, error: anyhow::Error, cx: &mut Context<Self>) {
        self.error = Some(format!("{error:#}"));
        if self.status == Status::Loading {
            self.status = Status::Ready;
        }
        cx.notify();
    }

    fn request(
        &self,
        method: &str,
        params: serde_json::Value,
        cx: &mut Context<Self>,
    ) -> Result<Task<Result<serde_json::Value>>> {
        anyhow::ensure!(
            self.status != Status::Closed,
            "this data frame is no longer available"
        );
        let comm_id = self.comm_id.clone();
        Ok(self.session.update(cx, |session, cx| {
            session.comm_request(&comm_id, method, params, REQUEST_TIMEOUT, cx)
        }))
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let shape = (self.data.shape.is_some()).then(|| {
            format!(
                "{} rows × {} columns",
                model::format_count(self.data.row_count()),
                model::format_count(self.data.column_count())
            )
        });
        let paging = (self.data.column_count() > PAGE_COLUMNS).then(|| {
            let columns = self.data.page_columns();
            h_flex()
                .gap_1()
                .child(
                    IconButton::new("previous-columns", IconName::ChevronLeft)
                        .icon_size(IconSize::Small)
                        .disabled(!self.data.can_page(false))
                        .tooltip(Tooltip::text("Previous Columns"))
                        .on_click(cx.listener(|this, _, _, cx| this.page_columns(false, cx))),
                )
                .child(
                    Label::new(format!(
                        "Columns {}–{} of {}",
                        columns.start + 1,
                        columns.end,
                        model::format_count(self.data.column_count())
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .child(
                    IconButton::new("next-columns", IconName::ChevronRight)
                        .icon_size(IconSize::Small)
                        .disabled(!self.data.can_page(true))
                        .tooltip(Tooltip::text("Next Columns"))
                        .on_click(cx.listener(|this, _, _, cx| this.page_columns(true, cx))),
                )
        });
        h_flex()
            .px_2()
            .py_1()
            .gap_3()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(Label::new(self.title.clone()))
            .children(
                shape.map(|shape| Label::new(shape).size(LabelSize::Small).color(Color::Muted)),
            )
            .when(self.status == Status::Loading, |this| {
                this.child(
                    Label::new("Loading…")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .when(self.status == Status::Closed, |this| {
                this.child(
                    Label::new("No longer live: the data frame or R is gone.")
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                )
            })
            .children(self.error.clone().map(|error| {
                Label::new(error)
                    .size(LabelSize::Small)
                    .color(Color::Error)
                    .truncate()
            }))
            .child(div().flex_1())
            .children(paging)
    }

    fn render_header(&self, page_column: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(column) = self.data.columns.get(page_column) else {
            return div().into_any_element();
        };
        let column_index = column.column_index;
        let sorted = self
            .data
            .sort
            .filter(|key| key.column_index == column_index)
            .map(|key| key.ascending);
        let mut details = column.type_name.clone();
        if let Some(count) = self.data.null_counts.get(&column_index)
            && *count > 0
        {
            details.push_str(&format!(" · {} NA", model::format_count(*count as usize)));
        }
        let tooltip = column
            .column_label
            .clone()
            .unwrap_or_else(|| column.column_name.clone());
        v_flex()
            .id(("column-header", page_column))
            .w_full()
            .cursor_pointer()
            .tooltip(Tooltip::text(tooltip))
            .on_click(cx.listener(move |this, _, _, cx| this.sort(column_index, cx)))
            .child(
                h_flex()
                    .gap_1()
                    .child(Label::new(column.column_name.clone()).truncate())
                    .children(sorted.map(|ascending| {
                        Icon::new(if ascending {
                            IconName::ArrowUp
                        } else {
                            IconName::ArrowDown
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Accent)
                    })),
            )
            .child(
                Label::new(details)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .truncate(),
            )
            .into_any_element()
    }

    fn render_rows(&mut self, rows: Range<usize>, cx: &mut Context<Self>) -> Vec<Vec<AnyElement>> {
        self.ensure_rows(rows.clone(), cx);
        let numeric: Vec<bool> = self
            .data
            .columns
            .iter()
            .map(|column| model::is_numeric(column.type_display))
            .collect();
        rows.map(|row| {
            let label = self.data.row_label(row).unwrap_or_default();
            let mut cells = Vec::with_capacity(numeric.len() + 1);
            cells.push(
                Label::new(label)
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate()
                    .into_any_element(),
            );
            for (page_column, right_aligned) in numeric.iter().enumerate() {
                let cell = match self.data.cell(row, page_column) {
                    Some(value) => {
                        let (text, special) = value.display();
                        Label::new(text.to_string())
                            .size(LabelSize::Small)
                            .when(special, |label| label.color(Color::Muted).italic())
                            .truncate()
                            .into_any_element()
                    }
                    None => Label::new(if self.data.block_error(row).is_some() {
                        ""
                    } else {
                        "…"
                    })
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element(),
                };
                cells.push(
                    h_flex()
                        .w_full()
                        .when(*right_aligned, |this| this.justify_end())
                        .child(cell)
                        .into_any_element(),
                );
            }
            cells
        })
        .collect()
    }
}

fn column_widths(columns: usize) -> ResizableColumnsState {
    let mut widths = vec![AbsoluteLength::Pixels(px(COLUMN_WIDTH)); columns + 1];
    widths[0] = AbsoluteLength::Pixels(px(72.));
    let mut behaviors = vec![TableResizeBehavior::Resizable; columns + 1];
    behaviors[0] = TableResizeBehavior::None;
    ResizableColumnsState::new(columns + 1, widths, behaviors)
}

impl Render for DataExplorerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let columns = self.data.columns.len();
        let row_count = self.data.row_count();
        let mut headers = Vec::with_capacity(columns + 1);
        headers.push(div().into_any_element());
        for page_column in 0..columns {
            headers.push(self.render_header(page_column, cx));
        }
        v_flex()
            .key_context("DataExplorer")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_toolbar(cx))
            .when(
                columns > 0 && self.widths.read(cx).cols() == columns + 1,
                |this| {
                    this.child(
                        Table::new(columns + 1)
                            .interactable(&self.interaction)
                            .width_config(ColumnWidthConfig::Resizable(self.widths.clone()))
                            .header(headers)
                            .pin_cols(1)
                            .striped()
                            .uniform_list(
                                "data-explorer-rows",
                                row_count,
                                cx.processor(|this, rows: Range<usize>, _, cx| {
                                    this.render_rows(rows, cx)
                                }),
                            ),
                    )
                },
            )
    }
}

impl Focusable for DataExplorerView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for DataExplorerView {}

impl Item for DataExplorerView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Table))
    }

    fn tab_tooltip_text(&self, _: &App) -> Option<SharedString> {
        Some(format!("Data viewer: {}", self.title).into())
    }

    /// Closing the tab tells Ark to let go of the data frame.
    fn on_removed(&self, cx: &mut Context<Self>) {
        if self.status != Status::Closed {
            let comm_id = self.comm_id.clone();
            self.session
                .update(cx, |session, _| session.close_comm(&comm_id))
                .log_err();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::channel::mpsc;
    use gpui::{TestAppContext, VisualTestContext};
    use jupyter_protocol::{
        CommId, CommMsg, CommOpen, JupyterMessage, JupyterMessageContent, KernelInfoReply,
    };
    use languages::r::{RInstallationSource, RToolchainData};
    use project::{FakeFs, Project};
    use runtime::comms::{ColumnValue, DATA_EXPLORER_COMM_TARGET};
    use serde_json::{Value, json};
    use settings::SettingsStore;
    use std::path::PathBuf;
    use workspace::{MultiWorkspace, SaveIntent};

    struct Harness {
        workspace: Entity<Workspace>,
        session: Entity<RSession>,
        requests: mpsc::Receiver<JupyterMessage>,
    }

    struct Request {
        comm_id: String,
        method: String,
        params: Value,
        message: JupyterMessage,
    }

    impl Harness {
        async fn new(cx: &mut TestAppContext) -> (Self, &mut VisualTestContext) {
            cx.update(|cx| {
                let settings = SettingsStore::test(cx);
                cx.set_global(settings);
                theme_settings::init(theme::LoadThemes::JustBase, cx);
            });
            let fs = FakeFs::new(cx.executor());
            let project = Project::test(fs, [] as [&std::path::Path; 0], cx).await;
            let window =
                cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
            let workspace = window
                .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
                .unwrap();
            let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
            let session = cx.new(|cx| {
                RSession::new(
                    Some(RToolchainData {
                        r_home: PathBuf::from("/opt/R/4.5.3/lib/R"),
                        version: Some("4.5.3".to_string()),
                        source: RInstallationSource::Rig,
                    }),
                    PathBuf::from("/projects/analysis"),
                    Default::default(),
                    None,
                    cx,
                )
            });
            let (requests_tx, mut requests) = mpsc::channel(100);
            let (stdin_tx, _stdin) = mpsc::channel(100);
            session.update(cx, |session, cx| {
                session.connect_for_test(requests_tx, stdin_tx, cx)
            });
            let kernel_info = requests.try_recv().unwrap();
            workspace.update_in(cx, |workspace, window, cx| {
                watch_session(workspace, &session, window, cx)
            });
            let mut harness = Self {
                workspace,
                session,
                requests,
            };
            let reply: KernelInfoReply = serde_json::from_value(json!({
                "status": "ok", "protocol_version": "5.3", "implementation": "ark",
                "implementation_version": "0.1.252",
                "language_info": { "name": "R", "version": "4.5.3" },
                "banner": "", "help_links": [], "debugger": false
            }))
            .unwrap();
            harness.receive(JupyterMessage::new(reply, Some(&kernel_info)), cx);
            // The UI, LSP and variables comms the session opens at startup.
            while harness.requests.try_recv().is_ok() {}
            (harness, cx)
        }

        fn receive(&self, message: JupyterMessage, cx: &mut VisualTestContext) {
            self.session.update_in(cx, |session, window, cx| {
                session.receive_for_test(&message, window, cx)
            });
            cx.run_until_parked();
        }

        /// Ark opening an explorer, as `View(df)` does.
        fn open(
            &self,
            comm_id: &str,
            title: &str,
            cx: &mut VisualTestContext,
        ) -> Entity<DataExplorerView> {
            let Value::Object(data) = json!({ "title": title, "inline_only": false }) else {
                unreachable!()
            };
            self.receive(
                JupyterMessage::new(
                    CommOpen {
                        comm_id: CommId(comm_id.to_string()),
                        target_name: DATA_EXPLORER_COMM_TARGET.to_string(),
                        data,
                        target_module: None,
                    },
                    None,
                ),
                cx,
            );
            self.view(title, cx).expect("a viewer tab")
        }

        fn view(
            &self,
            title: &str,
            cx: &mut VisualTestContext,
        ) -> Option<Entity<DataExplorerView>> {
            self.workspace.read_with(cx, |workspace, cx| {
                workspace
                    .items_of_type::<DataExplorerView>(cx)
                    .find(|view| view.read(cx).title.as_ref() == title)
            })
        }

        fn next(&mut self) -> Request {
            let message = self.requests.try_recv().expect("a request to Ark");
            let (comm_id, method, params) = match &message.content {
                JupyterMessageContent::CommMsg(comm_message) => (
                    comm_message.comm_id.0.clone(),
                    comm_message.data["method"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    comm_message.data.get("params").cloned().unwrap_or_default(),
                ),
                JupyterMessageContent::CommClose(close) => (
                    close.comm_id.0.clone(),
                    "comm_close".to_string(),
                    Value::Null,
                ),
                other => panic!("unexpected {}", other.message_type()),
            };
            Request {
                comm_id,
                method,
                params,
                message,
            }
        }

        /// Answers the next request, which must be `method`, and returns its params.
        fn answer(&mut self, method: &str, result: Value, cx: &mut VisualTestContext) -> Value {
            let request = self.next();
            assert_eq!(request.method, method);
            let Value::Object(data) = json!({ "method": method, "result": result }) else {
                unreachable!()
            };
            self.receive(
                JupyterMessage::new(
                    CommMsg {
                        comm_id: CommId(request.comm_id),
                        data,
                    },
                    Some(&request.message),
                ),
                cx,
            );
            request.params
        }

        fn event(&self, comm_id: &str, data: Value, cx: &mut VisualTestContext) {
            let Value::Object(data) = data else {
                panic!("event data must be an object");
            };
            self.receive(
                JupyterMessage::new(
                    CommMsg {
                        comm_id: CommId(comm_id.to_string()),
                        data,
                    },
                    None,
                ),
                cx,
            );
        }

        /// Answers what a viewer asks when it (re)loads: the shape, the
        /// columns, the missing-value counts, then the rows in view.
        fn load(&mut self, rows: i64, has_row_labels: bool, cx: &mut VisualTestContext) {
            self.answer("get_state", state(rows, has_row_labels, json!([])), cx);
            let params = self.answer("get_schema", schema(), cx);
            assert_eq!(params, json!({ "column_indices": [0, 1, 2] }));
            let params = self.answer("get_column_profiles", Value::Null, cx);
            assert_eq!(
                params["profiles"][2]["profiles"][0]["profile_type"],
                "null_count"
            );
            self.answer_block(0, rows.min(100), has_row_labels, cx);
        }

        fn answer_block(
            &mut self,
            first: i64,
            count: i64,
            has_row_labels: bool,
            cx: &mut VisualTestContext,
        ) {
            let params = self.answer("get_data_values", values(first, count), cx);
            assert_eq!(
                params["columns"][0]["spec"],
                json!({ "first_index": first, "last_index": first + count - 1 })
            );
            if has_row_labels {
                let labels: Vec<String> = (first..first + count)
                    .map(|row| format!("r{row}"))
                    .collect();
                self.answer("get_row_labels", json!({ "row_labels": [labels] }), cx);
            }
        }

        fn assert_no_requests(&mut self) {
            if let Ok(request) = self.requests.try_recv() {
                panic!("unexpected request {:?}", request.content);
            }
        }
    }

    fn state(rows: i64, has_row_labels: bool, sort_keys: Value) -> Value {
        json!({
            "display_name": "big",
            "table_shape": { "num_rows": rows, "num_columns": 3 },
            "table_unfiltered_shape": { "num_rows": rows, "num_columns": 3 },
            "has_row_labels": has_row_labels, "column_filters": [], "row_filters": [],
            "sort_keys": sort_keys, "supported_features": {}
        })
    }

    fn schema() -> Value {
        let column = |index: i64, name: &str, type_name: &str, display: &str| {
            json!({
                "column_name": name, "column_index": index,
                "type_name": type_name, "type_display": display
            })
        };
        json!({ "columns": [
            column(0, "id", "int", "integer"),
            column(1, "x", "dbl", "floating"),
            column(2, "g", "chr", "string"),
        ]})
    }

    /// Ids counting from 1, a constant, and NA in every other row.
    fn values(first: i64, count: i64) -> Value {
        let rows = first..first + count;
        json!({ "columns": [
            rows.clone().map(|row| (row + 1).to_string()).collect::<Vec<_>>(),
            rows.clone().map(|_| json!("0.5")).collect::<Vec<_>>(),
            rows.map(|row| if row % 2 == 0 { json!(1) } else { json!("a") }).collect::<Vec<_>>(),
        ]})
    }

    #[gpui::test]
    async fn test_opens_and_pages_rows_from_r(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        let view = harness.open("explorer-1", "big", cx);
        harness.load(10_000_000, true, cx);
        harness.assert_no_requests();

        let callback_id = view.read_with(cx, |view, _| view.profile_callback.clone().unwrap());
        harness.event(
            "explorer-1",
            json!({ "method": "return_column_profiles", "params": {
                "callback_id": callback_id,
                "profiles": [{ "null_count": 0 }, { "null_count": 0 }, { "null_count": 1234 }]
            }}),
            cx,
        );
        view.read_with(cx, |view, _| {
            assert_eq!(view.status, Status::Ready);
            assert_eq!(view.data.row_count(), 10_000_000);
            assert_eq!(view.data.null_counts.get(&2), Some(&1234));
            assert_eq!(view.data.row_label(3).as_deref(), Some("r3"));
        });

        // Scrolling to the middle fetches only that block.
        view.update(cx, |view, cx| view.ensure_rows(5_000_010..5_000_040, cx));
        harness.answer_block(5_000_000, 100, true, cx);
        harness.assert_no_requests();
        view.read_with(cx, |view, _| {
            let cell = |column| view.data.cell(5_000_010, column).map(ColumnValue::display);
            assert_eq!(cell(0), Some(("5000011", false)));
            assert_eq!(cell(2), Some(("NA", true)));
            assert_eq!(view.data.row_label(5_000_010).as_deref(), Some("r5000010"));
        });

        // A long jump asks for at most four blocks at once; the rest wait.
        view.update(cx, |view, cx| view.ensure_rows(6_000_000..6_000_600, cx));
        let mut fetched = Vec::new();
        while let Ok(request) = harness.requests.try_recv() {
            if let JupyterMessageContent::CommMsg(message) = &request.content
                && message.data["method"] == "get_data_values"
            {
                fetched.push(message.data["params"]["columns"][0]["spec"]["first_index"].clone());
            }
        }
        assert_eq!(fetched, [6_000_000, 6_000_100, 6_000_200, 6_000_300]);
    }

    #[gpui::test]
    async fn test_sorting_and_updates_refetch(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        let view = harness.open("explorer-1", "big", cx);
        harness.load(10_000_000, false, cx);

        view.update(cx, |view, cx| view.sort(0, cx));
        let params = harness.answer("set_sort_columns", Value::Null, cx);
        assert_eq!(
            params,
            json!({ "sort_keys": [{ "column_index": 0, "ascending": true }] })
        );
        // No rows are fetched until R has sorted, then the shape and rows are fetched again.
        harness.answer(
            "get_state",
            state(
                10_000_000,
                false,
                json!([{ "column_index": 0, "ascending": true }]),
            ),
            cx,
        );
        harness.answer("get_schema", schema(), cx);
        harness.answer("get_column_profiles", Value::Null, cx);
        harness.answer_block(0, 100, false, cx);
        harness.assert_no_requests();
        assert_eq!(
            view.read_with(cx, |view, _| view.data.sort),
            Some(ColumnSortKey {
                column_index: 0,
                ascending: true
            })
        );

        // When the data frame changes in R, the new shape bounds the rows fetched.
        harness.event(
            "explorer-1",
            json!({ "method": "data_update", "params": {} }),
            cx,
        );
        harness.load(42, false, cx);
        harness.assert_no_requests();
        view.read_with(cx, |view, _| {
            assert_eq!(view.data.row_count(), 42);
            assert_eq!(view.data.sort, None);
            assert!(view.data.cell(41, 0).is_some());
        });
    }

    #[gpui::test]
    async fn test_reuse_close_and_r_stopping(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        let view = harness.open("explorer-1", "big", cx);
        harness.load(10, false, cx);

        // View() again on the same data frame reuses its tab.
        let same_view = harness.open("explorer-2", "big", cx);
        assert_eq!(same_view.entity_id(), view.entity_id());
        let close = harness.next();
        assert_eq!(
            (close.comm_id.as_str(), close.method.as_str()),
            ("explorer-1", "comm_close")
        );
        harness.load(10, false, cx);
        let tabs = harness.workspace.read_with(cx, |workspace, cx| {
            workspace.items_of_type::<DataExplorerView>(cx).count()
        });
        assert_eq!(tabs, 1);

        // Closing the tab lets Ark release the data frame.
        let pane = harness
            .workspace
            .read_with(cx, |workspace, _| workspace.active_pane().clone());
        pane.update_in(cx, |pane, window, cx| {
            pane.close_item_by_id(view.entity_id(), SaveIntent::Skip, window, cx)
        })
        .await
        .unwrap();
        let close = harness.next();
        assert_eq!(
            (close.comm_id.as_str(), close.method.as_str()),
            ("explorer-2", "comm_close")
        );
        assert!(harness.view("big", cx).is_none());

        // A viewer whose R stops says it's no longer live and asks nothing more.
        let other = harness.open("explorer-3", "iris", cx);
        harness.load(10, false, cx);
        harness
            .session
            .update(cx, |session, cx| session.stop(cx))
            .detach();
        cx.run_until_parked();
        while harness.requests.try_recv().is_ok() {}
        assert_eq!(
            other.read_with(cx, |view, _| view.status.clone()),
            Status::Closed
        );
        other.update(cx, |view, cx| view.sort(0, cx));
        drop(other);
        harness.assert_no_requests();
    }
}
