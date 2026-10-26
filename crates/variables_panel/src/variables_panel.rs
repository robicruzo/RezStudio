//! The Environment pane: the R session's variables, live, like RStudio's.

mod search_path;
mod tree;

use anyhow::{Context as _, Result};
use editor::Editor;
use gpui::{
    Action, Anchor, App, AsyncWindowContext, Empty, Entity, EntityId, EventEmitter, FocusHandle,
    Focusable, Pixels, PromptLevel, Subscription, Task, WeakEntity, Window, actions, px,
    uniform_list,
};
use runtime::comms::{InspectedVariable, VariablesEvent};
use runtime::{RSession, SessionAdded, SessionEvent, SessionRegistry, SessionState};
use std::ops::Range;
use std::time::Duration;
use tree::{Row, VariablePath, VariableTree};
use ui::{
    ContextMenu, ContextMenuEntry, Disclosure, IconButton, IconName, PopoverMenu, Tooltip,
    prelude::*,
};
use util::ResultExt as _;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

actions!(
    variables_panel,
    [
        /// Opens the Environment pane and moves focus to it.
        ToggleFocus,
        /// Removes every variable from the R session's global environment.
        ClearEnvironment,
    ]
);

const PANEL_KEY: &str = "EnvironmentPanel";
/// Ark answers on R's thread, so a request can wait for running code.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<EnvironmentPanel>(window, cx);
        });
    })
    .detach();
}

pub struct EnvironmentPanel {
    workspace_id: EntityId,
    session: Option<Entity<RSession>>,
    /// The global environment, kept live by Ark's variables comm.
    tree: VariableTree,
    /// The `package:` entries on R's search path, for the environment menu.
    attached_packages: Vec<String>,
    /// The package environment shown instead of the global one, if any.
    package: Option<PackageEnvironment>,
    rows: Vec<Row>,
    filter: Entity<Editor>,
    focus_handle: FocusHandle,
    position: DockPosition,
    /// The last failed request, shown under the toolbar.
    error: Option<String>,
    _session_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl EnvironmentPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            cx.new(|cx| Self::new(workspace, window, cx))
        })
    }

    pub fn new(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let workspace_id = workspace.weak_handle().entity_id();
        let filter = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter by name", window, cx);
            editor
        });
        let mut subscriptions = vec![cx.subscribe(&filter, |this, _, event, cx| {
            if let editor::EditorEvent::BufferEdited = event {
                this.rebuild_rows(cx);
            }
        })];
        if let Some(registry) = SessionRegistry::global(cx) {
            subscriptions.push(cx.subscribe_in(
                &registry,
                window,
                |this, _, event: &SessionAdded, window, cx| {
                    if event.workspace_id == this.workspace_id {
                        this.set_session(event.session.clone(), window, cx);
                    }
                },
            ));
        }
        let mut this = Self {
            workspace_id,
            session: None,
            tree: VariableTree::default(),
            attached_packages: Vec::new(),
            package: None,
            rows: Vec::new(),
            filter,
            focus_handle: cx.focus_handle(),
            position: DockPosition::Right,
            error: None,
            _session_subscription: None,
            _subscriptions: subscriptions,
        };
        let existing = SessionRegistry::global(cx)
            .and_then(|registry| registry.read(cx).session(workspace_id).cloned());
        if let Some(session) = existing {
            this.set_session(session, window, cx);
        }
        this
    }

    pub fn set_session(
        &mut self,
        session: Entity<RSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._session_subscription =
            Some(cx.subscribe_in(&session, window, |this, _, event, _, cx| {
                this.handle_session_event(event, cx)
            }));
        self.session = Some(session);
        self.tree.clear();
        self.attached_packages.clear();
        self.package = None;
        self.rebuild_rows(cx);
    }

    fn handle_session_event(&mut self, event: &SessionEvent, cx: &mut Context<Self>) {
        match event {
            SessionEvent::Variables(VariablesEvent::Refresh(refresh)) => {
                let refetch = self.tree.refresh(refresh.variables.clone());
                self.error = None;
                self.fetch_children(refetch, cx);
                self.rebuild_rows(cx);
                self.refresh_attached_packages(cx);
            }
            // Code can attach or detach packages without touching the global
            // environment, so there's no variables update to go by.
            SessionEvent::ExecutionFinished { .. } => self.refresh_attached_packages(cx),
            SessionEvent::Variables(VariablesEvent::Update(update)) => {
                let refetch = self.tree.update(update.assigned.clone(), &update.removed);
                self.fetch_children(refetch, cx);
                self.rebuild_rows(cx);
            }
            SessionEvent::StateChanged => {
                let running = self.session.as_ref().is_some_and(|session| {
                    matches!(
                        session.read(cx).state(),
                        SessionState::Idle | SessionState::Busy
                    )
                });
                // A new R process sends its own refresh once it starts.
                if !running && (!self.tree.is_empty() || self.package.is_some()) {
                    self.tree.clear();
                    self.attached_packages.clear();
                    self.package = None;
                    self.rebuild_rows(cx);
                }
                cx.notify();
            }
            _ => {}
        }
    }

    fn rebuild_rows(&mut self, cx: &mut Context<Self>) {
        let filter = self.filter.read(cx).text(cx);
        self.rows = match &self.package {
            Some(package) => package.tree.rows(&filter),
            None => self.tree.rows(&filter),
        };
        cx.notify();
    }

    fn refresh_attached_packages(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let reply = session.update(cx, |session, cx| {
            session.evaluate_code(search_path::SEARCH_PATH_CODE, cx)
        });
        cx.spawn(async move |this, cx| {
            let attached = reply
                .await
                .and_then(|reply| search_path::attached_packages(reply.result));
            this.update(cx, |this, cx| match attached {
                Ok(attached) => this.set_attached_packages(attached, cx),
                Err(error) => log::error!("listing R's search path: {error:#}"),
            })
            .log_err();
        })
        .detach();
    }

    fn set_attached_packages(&mut self, attached: Vec<String>, cx: &mut Context<Self>) {
        // As in RStudio, detaching the package being shown goes back to the
        // global environment.
        if self
            .package
            .as_ref()
            .is_some_and(|package| !attached.contains(&package.search_name))
        {
            self.package = None;
            self.rebuild_rows(cx);
        }
        self.attached_packages = attached;
        cx.notify();
    }

    /// Shows a package environment, such as `package:stats`, or the global
    /// environment for `None`.
    fn select_environment(&mut self, search_name: Option<String>, cx: &mut Context<Self>) {
        self.error = None;
        let Some(search_name) = search_name else {
            self.package = None;
            self.rebuild_rows(cx);
            return;
        };
        let Some(session) = self.session.clone() else {
            return;
        };
        let reply = session.update(cx, |session, cx| {
            session.evaluate_code(&search_path::listing_code(&search_name), cx)
        });
        self.package = Some(PackageEnvironment {
            search_name: search_name.clone(),
            tree: VariableTree::default(),
            loading: true,
        });
        self.rebuild_rows(cx);
        cx.spawn(async move |this, cx| {
            let listing = reply
                .await
                .and_then(|reply| search_path::parse_listing(reply.result));
            this.update(cx, |this, cx| {
                // A later choice replaces this one.
                let Some(package) = this
                    .package
                    .as_mut()
                    .filter(|package| package.search_name == search_name)
                else {
                    return;
                };
                package.loading = false;
                match listing {
                    Ok(variables) => {
                        package.tree.refresh(variables);
                    }
                    Err(error) => this.error = Some(format!("{error:#}")),
                }
                this.rebuild_rows(cx);
            })
            .log_err();
        })
        .detach();
    }

    fn render_environment_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (label, icon) = match &self.package {
            Some(package) => (
                SharedString::from(package.search_name.clone()),
                IconName::Blocks,
            ),
            None => (SharedString::from("Global Environment"), IconName::Box),
        };
        let panel = cx.weak_entity();
        let attached_packages = self.attached_packages.clone();
        PopoverMenu::new("environment-menu")
            .trigger_with_tooltip(
                Button::new("environment-menu-trigger", label)
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
                    .end_icon(
                        Icon::new(IconName::ChevronDown)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .disabled(self.session.is_none()),
                Tooltip::text("Choose an Environment"),
            )
            .anchor(Anchor::TopLeft)
            .menu(move |window, cx| {
                let panel = panel.clone();
                let attached_packages = attached_packages.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    let entry = |label: &str, icon: IconName, search_name: Option<String>| {
                        let panel = panel.clone();
                        ContextMenuEntry::new(label.to_string())
                            .icon(icon)
                            .icon_position(IconPosition::Start)
                            .icon_color(Color::Muted)
                            .handler(move |_, cx| {
                                panel
                                    .update(cx, |panel, cx| {
                                        panel.select_environment(search_name.clone(), cx)
                                    })
                                    .log_err();
                            })
                    };
                    let mut menu = menu.item(entry("Global Environment", IconName::Box, None));
                    for search_name in &attached_packages {
                        menu = menu.item(entry(
                            search_name,
                            IconName::Blocks,
                            Some(search_name.clone()),
                        ));
                    }
                    menu
                }))
            })
    }

    fn toggle(&mut self, path: VariablePath, cx: &mut Context<Self>) {
        if self.tree.toggle(&path) {
            self.fetch_children(vec![path], cx);
        }
        self.rebuild_rows(cx);
    }

    fn fetch_children(&mut self, paths: Vec<VariablePath>, cx: &mut Context<Self>) {
        for path in paths {
            let request = self.request("inspect", serde_json::json!({ "path": path }), cx);
            cx.spawn(async move |this, cx| {
                let result = match request {
                    Ok(request) => request.await.and_then(|result| {
                        serde_json::from_value::<InspectedVariable>(result)
                            .context("unexpected reply to inspect")
                    }),
                    Err(error) => Err(error),
                };
                this.update(cx, |this, cx| {
                    match result {
                        Ok(inspected) => {
                            this.tree
                                .set_children(&path, inspected.children, inspected.length)
                        }
                        Err(error) => this.tree.set_children_error(&path, format!("{error:#}")),
                    }
                    this.rebuild_rows(cx);
                })
                .log_err();
            })
            .detach();
        }
    }

    /// Opens the variable in the data viewer; Ark answers by opening a data explorer.
    fn view(&mut self, path: VariablePath, cx: &mut Context<Self>) {
        self.send_and_report("view", serde_json::json!({ "path": path }), cx);
    }

    fn delete(&mut self, name: String, cx: &mut Context<Self>) {
        self.send_and_report("delete", serde_json::json!({ "names": [name] }), cx);
    }

    fn clear_environment(
        &mut self,
        _: &ClearEnvironment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tree.is_empty() || self.package.is_some() {
            return;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            "Remove all objects from the environment?",
            Some("This removes every variable in the global environment. It can't be undone."),
            &["Remove All", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await.ok() == Some(0) {
                this.update(cx, |this, cx| {
                    this.send_and_report(
                        "clear",
                        serde_json::json!({ "include_hidden_objects": false }),
                        cx,
                    )
                })
                .log_err();
            }
        })
        .detach();
    }

    /// Sends a request whose effect arrives as a variables update, showing
    /// any error in the pane.
    fn send_and_report(
        &mut self,
        method: &'static str,
        params: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        let request = self.request(method, params, cx);
        cx.spawn(async move |this, cx| {
            let result = match request {
                Ok(request) => request.await.map(|_| ()),
                Err(error) => Err(error),
            };
            this.update(cx, |this, cx| {
                this.error = result.err().map(|error| format!("{error:#}"));
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    fn request(
        &self,
        method: &str,
        params: serde_json::Value,
        cx: &mut Context<Self>,
    ) -> Result<Task<Result<serde_json::Value>>> {
        let session = self.session.clone().context("R isn't running")?;
        session.update(cx, |session, cx| {
            let comm_id = session
                .variables_comm_id()
                .context("R isn't running")?
                .to_string();
            Ok(session.comm_request(&comm_id, method, params, REQUEST_TIMEOUT, cx))
        })
    }

    fn render_row(&self, index: usize, row: &Row, cx: &mut Context<Self>) -> AnyElement {
        let muted = |text: String| Label::new(text).size(LabelSize::Small).color(Color::Muted);
        let note = |depth: usize, label: Label| {
            row_layout(
                ("environment-row", index),
                depth,
                None,
                label,
                Empty,
                Empty,
                Empty,
                None,
            )
        };
        match row {
            Row::Loading { depth } => note(*depth, muted("Loading…".to_string())),
            Row::Error { depth, message } => note(
                *depth,
                Label::new(message.clone())
                    .size(LabelSize::Small)
                    .color(Color::Error),
            ),
            Row::More { depth, count } => note(*depth, muted(format!("… and {count} more"))),
            Row::Variable {
                path,
                depth,
                variable,
                expanded,
            } => {
                let disclosure = variable.has_children.then(|| {
                    let toggle_path = path.clone();
                    Disclosure::new(("toggle-variable", index), *expanded)
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.toggle(toggle_path.clone(), cx)),
                        )
                        .into_any_element()
                });
                let mut actions = h_flex().gap_0p5().justify_end();
                if variable.has_viewer {
                    let view_path = path.clone();
                    actions = actions.child(
                        IconButton::new(("view-variable", index), IconName::Table)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("View in Data Viewer"))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.view(view_path.clone(), cx)),
                            ),
                    );
                }
                if *depth == 0 && self.package.is_none() {
                    let name = variable.display_name.clone();
                    actions = actions.child(
                        IconButton::new(("delete-variable", index), IconName::Trash)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Remove from Environment"))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.delete(name.clone(), cx)),
                            ),
                    );
                }
                row_layout(
                    ("environment-row", index),
                    *depth,
                    disclosure,
                    Label::new(variable.display_name.clone()).truncate(),
                    muted(variable.display_type.clone()).truncate(),
                    muted(tree::format_size(variable.size)),
                    muted(variable.display_value.clone()).truncate(),
                    Some((
                        actions.into_any_element(),
                        cx.theme().colors().panel_background,
                    )),
                )
            }
        }
    }

    fn placeholder(&self, cx: &App) -> Option<&'static str> {
        let Some(session) = &self.session else {
            return Some("R isn't running in this window.");
        };
        match session.read(cx).state() {
            SessionState::Idle | SessionState::Busy => {
                if let Some(package) = &self.package {
                    return if package.loading {
                        Some("Loading…")
                    } else if package.tree.is_empty() {
                        Some("This environment is empty.")
                    } else if self.rows.is_empty() {
                        Some("No objects match the filter.")
                    } else {
                        None
                    };
                }
                if self.rows.is_empty() {
                    if self.tree.is_empty() {
                        Some("Variables you create in R appear here.")
                    } else {
                        Some("No variables match the filter.")
                    }
                } else {
                    None
                }
            }
            SessionState::Starting | SessionState::Restarting => Some("Starting R…"),
            SessionState::Restricted => Some("R starts once you trust this folder."),
            SessionState::Unavailable(_) | SessionState::Exited(_) => Some("R isn't running."),
        }
    }
}

struct PackageEnvironment {
    /// The name on R's search path, such as `package:stats`.
    search_name: String,
    tree: VariableTree,
    loading: bool,
}

const ROW_HEIGHT: Pixels = px(26.);
const INDENT: Pixels = px(12.);
const DISCLOSURE_WIDTH: Pixels = px(18.);
const SIZE_WIDTH: Pixels = px(60.);

/// One line of the pane. The heading and every row share these columns so
/// they line up; nesting only indents the name. Row actions float over the
/// end of the row on hover rather than taking room from the value.
#[allow(clippy::too_many_arguments)]
fn row_layout(
    id: impl Into<ElementId>,
    depth: usize,
    disclosure: Option<AnyElement>,
    name: impl IntoElement,
    type_name: impl IntoElement,
    size: impl IntoElement,
    value: impl IntoElement,
    actions: Option<(AnyElement, gpui::Hsla)>,
) -> AnyElement {
    h_flex()
        .id(id)
        .group("environment-row")
        .relative()
        .h(ROW_HEIGHT)
        .w_full()
        .px_2()
        .gap_2()
        .child(
            h_flex()
                .w(relative(0.3))
                .min_w_0()
                .pl(INDENT * depth as f32)
                .child(div().w(DISCLOSURE_WIDTH).flex_none().children(disclosure))
                .child(div().min_w_0().child(name)),
        )
        .child(div().w(relative(0.18)).min_w_0().child(type_name))
        .child(div().w(SIZE_WIDTH).flex_none().child(size))
        .child(div().flex_1().min_w_0().child(value))
        .children(actions.map(|(actions, background)| {
            h_flex()
                .absolute()
                .right_1()
                .h_full()
                .pl_1()
                .bg(background)
                .visible_on_hover("environment-row")
                .child(actions)
        }))
        .into_any_element()
}

impl Render for EnvironmentPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let placeholder = self.placeholder(cx);
        let row_count = self.rows.len();
        v_flex()
            .key_context("EnvironmentPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::clear_environment))
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(self.render_environment_menu(cx))
                    .child(div().flex_1().min_w_0().child(self.filter.clone()))
                    .child(
                        IconButton::new("clear-environment", IconName::Trash)
                            .icon_size(IconSize::Small)
                            .disabled(self.tree.is_empty() || self.package.is_some())
                            .tooltip(Tooltip::text("Remove All Objects"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.clear_environment(&ClearEnvironment, window, cx)
                            })),
                    ),
            )
            .when(placeholder.is_none(), |this| {
                let heading = |text: &'static str| {
                    Label::new(text)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .weight(gpui::FontWeight::MEDIUM)
                        .truncate()
                };
                this.child(
                    div()
                        .border_b_1()
                        .border_color(cx.theme().colors().border_variant)
                        .child(row_layout(
                            "environment-heading",
                            0,
                            None,
                            heading("Name"),
                            heading("Type"),
                            heading("Size"),
                            heading("Value"),
                            None,
                        )),
                )
            })
            .children(self.error.clone().map(|error| {
                div()
                    .px_2()
                    .py_1()
                    .child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            }))
            .child(match placeholder {
                Some(message) => div()
                    .p_4()
                    .child(Label::new(message).color(Color::Muted))
                    .into_any_element(),
                None => uniform_list(
                    "environment-rows",
                    row_count,
                    cx.processor(|this, range: Range<usize>, _, cx| {
                        let rows = this
                            .rows
                            .get(range.clone())
                            .map(<[Row]>::to_vec)
                            .unwrap_or_default();
                        rows.iter()
                            .zip(range)
                            .map(|(row, index)| this.render_row(index, row, cx))
                            .collect::<Vec<_>>()
                    }),
                )
                .flex_1()
                .into_any_element(),
            })
    }
}

impl Focusable for EnvironmentPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for EnvironmentPanel {}

impl Panel for EnvironmentPanel {
    fn persistent_name() -> &'static str {
        "Environment"
    }

    fn panel_key() -> &'static str {
        PANEL_KEY
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(360.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::Box)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Environment")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn starts_open(&self, _: &Window, _: &App) -> bool {
        true
    }

    fn activation_priority(&self) -> u32 {
        9
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::channel::mpsc;
    use gpui::{TestAppContext, VisualTestContext};
    use jupyter_protocol::{
        CommId, CommMsg, JupyterMessage, JupyterMessageContent, KernelInfoReply,
    };
    use languages::r::{RInstallationSource, RToolchainData};
    use project::{FakeFs, Project};
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::PathBuf;
    use workspace::MultiWorkspace;

    struct Harness {
        panel: Entity<EnvironmentPanel>,
        session: Entity<RSession>,
        requests: mpsc::Receiver<JupyterMessage>,
        variables_comm: CommId,
    }

    impl Harness {
        async fn new(cx: &mut TestAppContext) -> (Self, &mut VisualTestContext) {
            cx.update(|cx| {
                let settings = SettingsStore::test(cx);
                cx.set_global(settings);
                theme_settings::init(theme::LoadThemes::JustBase, cx);
                editor::init(cx);
            });
            let fs = FakeFs::new(cx.executor());
            let project = Project::test(fs, [] as [&std::path::Path; 0], cx).await;
            let window =
                cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
            let workspace = window
                .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
                .unwrap();
            let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
            let panel = workspace.update_in(cx, |workspace, window, cx| {
                let panel = cx.new(|cx| EnvironmentPanel::new(workspace, window, cx));
                workspace.add_panel(panel.clone(), window, cx);
                workspace.open_panel::<EnvironmentPanel>(window, cx);
                panel
            });
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
            panel.update_in(cx, |panel, window, cx| {
                panel.set_session(session.clone(), window, cx)
            });
            let kernel_info = requests.try_recv().unwrap();
            while requests.try_recv().is_ok() {}
            let variables_comm = CommId(session.read_with(cx, |session, _| {
                session.variables_comm_id().unwrap().to_string()
            }));
            let harness = Self {
                panel,
                session,
                requests,
                variables_comm,
            };
            let reply: KernelInfoReply = serde_json::from_value(json!({
                "status": "ok", "protocol_version": "5.3", "implementation": "ark",
                "implementation_version": "0.1.252", "language_info": { "name": "R", "version": "4.5.3" },
                "banner": "", "help_links": [], "debugger": false
            }))
            .unwrap();
            harness.receive(JupyterMessage::new(reply, Some(&kernel_info)), cx);
            (harness, cx)
        }

        fn receive(&self, message: JupyterMessage, cx: &mut VisualTestContext) {
            self.session.update_in(cx, |session, window, cx| {
                session.receive_for_test(&message, window, cx)
            });
            cx.run_until_parked();
        }

        fn send_variables_event(&self, data: serde_json::Value, cx: &mut VisualTestContext) {
            let serde_json::Value::Object(data) = data else {
                panic!("event data must be an object");
            };
            self.receive(
                JupyterMessage::new(
                    CommMsg {
                        comm_id: self.variables_comm.clone(),
                        data,
                    },
                    None,
                ),
                cx,
            );
        }

        /// Answers the next comm request with `result`, returning its method and params.
        fn answer(
            &mut self,
            result: serde_json::Value,
            cx: &mut VisualTestContext,
        ) -> (String, serde_json::Value) {
            let request = self.requests.try_recv().expect("a comm request");
            let JupyterMessageContent::CommMsg(comm_message) = &request.content else {
                panic!("expected comm_msg, got {}", request.header.msg_type);
            };
            let method = comm_message.data["method"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let params = comm_message.data.get("params").cloned().unwrap_or_default();
            let serde_json::Value::Object(reply) = json!({ "method": method, "result": result })
            else {
                unreachable!()
            };
            self.receive(
                JupyterMessage::new(
                    CommMsg {
                        comm_id: comm_message.comm_id.clone(),
                        data: reply,
                    },
                    Some(&request),
                ),
                cx,
            );
            (method, params)
        }

        /// Answers the search path request that follows a refresh or an
        /// execution, with `packages` attached.
        fn answer_search_path(&mut self, packages: &[&str], cx: &mut VisualTestContext) {
            let search: Vec<&str> = std::iter::once(".GlobalEnv")
                .chain(packages.iter().copied())
                .chain(["Autoloads", "package:base"])
                .collect();
            let (method, params) = self.answer(json!({ "result": search, "output": "" }), cx);
            assert_eq!(
                (method.as_str(), params),
                ("evaluate_code", json!({ "code": "search()" }))
            );
        }

        fn row_names(&self, cx: &mut VisualTestContext) -> Vec<String> {
            self.panel.read_with(cx, |panel, _| {
                panel
                    .rows
                    .iter()
                    .map(|row| match row {
                        Row::Variable {
                            variable, depth, ..
                        } => {
                            format!("{}{}", "  ".repeat(*depth), variable.display_name)
                        }
                        Row::Loading { .. } => "…".to_string(),
                        Row::Error { message, .. } => format!("! {message}"),
                        Row::More { count, .. } => format!("+{count}"),
                    })
                    .collect()
            })
        }
    }

    fn variable(name: &str, has_children: bool, has_viewer: bool) -> serde_json::Value {
        json!({
            "access_key": name, "display_name": name, "display_value": format!("value of {name}"),
            "display_type": if has_viewer { "data.frame [32, 11]" } else { "num" }, "type_info": "",
            "size": 7208, "kind": if has_viewer { "table" } else { "number" }, "length": 1,
            "has_children": has_children, "has_viewer": has_viewer, "is_truncated": false, "updated_time": 0
        })
    }

    #[gpui::test]
    async fn test_shows_refreshes_and_updates(cx: &mut TestAppContext) {
        let (harness, cx) = Harness::new(cx).await;
        harness.send_variables_event(
            json!({ "method": "refresh", "params": {
                "variables": [variable("x", false, false), variable("cars", true, true)],
                "length": 2, "version": 1
            }}),
            cx,
        );
        assert_eq!(harness.row_names(cx), ["cars", "x"]);

        harness.send_variables_event(
            json!({ "method": "update", "params": {
                "assigned": [variable("fit", true, false)], "unevaluated": [], "removed": ["x"], "version": 2
            }}),
            cx,
        );
        assert_eq!(harness.row_names(cx), ["cars", "fit"]);

        harness.panel.update_in(cx, |panel, window, cx| {
            panel
                .filter
                .update(cx, |editor, cx| editor.set_text("fi", window, cx))
        });
        cx.run_until_parked();
        assert_eq!(harness.row_names(cx), ["fit"]);
    }

    #[gpui::test]
    async fn test_expanding_inspects_the_variable(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        harness.send_variables_event(
            json!({ "method": "refresh", "params": {
                "variables": [variable("cars", true, true)], "length": 1, "version": 1
            }}),
            cx,
        );
        harness.answer_search_path(&[], cx);
        harness
            .panel
            .update(cx, |panel, cx| panel.toggle(vec!["cars".to_string()], cx));
        assert_eq!(harness.row_names(cx), ["cars", "…"]);

        let (method, params) = harness.answer(
            json!({ "children": [variable("mpg", false, false), variable("cyl", false, false)], "length": 11 }),
            cx,
        );
        assert_eq!(method, "inspect");
        assert_eq!(params, json!({ "path": ["cars"] }));
        assert_eq!(harness.row_names(cx), ["cars", "  mpg", "  cyl", "+9"]);
    }

    #[gpui::test]
    async fn test_view_delete_and_clear_send_requests(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        harness.send_variables_event(
            json!({ "method": "refresh", "params": {
                "variables": [variable("cars", true, true)], "length": 1, "version": 1
            }}),
            cx,
        );
        harness.answer_search_path(&[], cx);

        harness
            .panel
            .update(cx, |panel, cx| panel.view(vec!["cars".to_string()], cx));
        let (method, params) = harness.answer(json!(null), cx);
        assert_eq!(
            (method.as_str(), params),
            ("view", json!({ "path": ["cars"] }))
        );

        harness
            .panel
            .update(cx, |panel, cx| panel.delete("cars".to_string(), cx));
        let (method, params) = harness.answer(json!(null), cx);
        assert_eq!(
            (method.as_str(), params),
            ("delete", json!({ "names": ["cars"] }))
        );

        // Clearing asks first; cancelling sends nothing.
        harness.panel.update_in(cx, |panel, window, cx| {
            panel.clear_environment(&ClearEnvironment, window, cx)
        });
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(harness.requests.try_recv().is_err());
        harness.panel.update_in(cx, |panel, window, cx| {
            panel.clear_environment(&ClearEnvironment, window, cx)
        });
        cx.simulate_prompt_answer("Remove All");
        cx.run_until_parked();
        let (method, params) = harness.answer(json!(null), cx);
        assert_eq!(
            (method.as_str(), params),
            ("clear", json!({ "include_hidden_objects": false }))
        );
    }

    #[gpui::test]
    async fn test_errors_are_shown_and_list_clears_when_r_stops(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        harness.send_variables_event(
            json!({ "method": "refresh", "params": {
                "variables": [variable("cars", true, true)], "length": 1, "version": 1
            }}),
            cx,
        );
        harness.answer_search_path(&[], cx);
        harness
            .panel
            .update(cx, |panel, cx| panel.delete("cars".to_string(), cx));
        let request = harness.requests.try_recv().unwrap();
        let JupyterMessageContent::CommMsg(comm_message) = &request.content else {
            panic!("expected comm_msg");
        };
        let serde_json::Value::Object(reply) = json!({ "error": { "code": -32603, "message": "cannot remove bindings from a locked environment" } })
        else {
            unreachable!()
        };
        harness.receive(
            JupyterMessage::new(
                CommMsg {
                    comm_id: comm_message.comm_id.clone(),
                    data: reply,
                },
                Some(&request),
            ),
            cx,
        );
        assert!(harness.panel.read_with(cx, |panel, _| {
            panel
                .error
                .as_deref()
                .is_some_and(|error| error.contains("locked environment"))
        }));

        harness
            .session
            .update(cx, |session, cx| session.stop(cx))
            .detach();
        cx.run_until_parked();
        assert!(harness.row_names(cx).is_empty());
        assert_eq!(
            harness
                .panel
                .read_with(cx, |panel, cx| panel.placeholder(cx)),
            Some("R isn't running.")
        );
    }

    fn function_record(name: &str) -> serde_json::Value {
        json!({
            "access_key": name, "display_name": name, "display_value": "function (x, ...)",
            "display_type": "function", "type_info": "", "size": 4848, "kind": "function",
            "length": 1, "has_children": false, "has_viewer": false, "is_truncated": false
        })
    }

    #[gpui::test]
    async fn test_environment_menu_shows_attached_packages(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        harness.send_variables_event(
            json!({ "method": "refresh", "params": {
                "variables": [variable("x", false, false)], "length": 1, "version": 1
            }}),
            cx,
        );
        harness.answer_search_path(&["package:stats"], cx);
        assert_eq!(
            harness
                .panel
                .read_with(cx, |panel, _| panel.attached_packages.clone()),
            ["package:stats", "package:base"]
        );

        // A package environment is listed by R code and shown read-only.
        harness.panel.update(cx, |panel, cx| {
            panel.select_environment(Some("package:stats".to_string()), cx)
        });
        assert!(harness.row_names(cx).is_empty());
        assert_eq!(
            harness
                .panel
                .read_with(cx, |panel, cx| panel.placeholder(cx)),
            Some("Loading…")
        );
        let (method, params) = harness.answer(
            json!({ "result": [function_record("lm"), function_record("acf")], "output": "" }),
            cx,
        );
        assert_eq!(method, "evaluate_code");
        assert!(
            params["code"]
                .as_str()
                .is_some_and(|code| code.contains(r#"search_name <- "package:stats""#)),
            "{params}"
        );
        assert_eq!(harness.row_names(cx), ["acf", "lm"]);
        harness.panel.update_in(cx, |panel, window, cx| {
            panel.clear_environment(&ClearEnvironment, window, cx)
        });
        assert!(!cx.has_pending_prompt());

        harness.panel.update_in(cx, |panel, window, cx| {
            panel
                .filter
                .update(cx, |editor, cx| editor.set_text("zzz", window, cx))
        });
        cx.run_until_parked();
        assert_eq!(
            harness
                .panel
                .read_with(cx, |panel, cx| panel.placeholder(cx)),
            Some("No objects match the filter.")
        );
        harness.panel.update_in(cx, |panel, window, cx| {
            panel
                .filter
                .update(cx, |editor, cx| editor.set_text("", window, cx))
        });
        cx.run_until_parked();

        // Changes to the global environment arrive while a package is shown,
        // and appear on going back.
        harness.send_variables_event(
            json!({ "method": "update", "params": {
                "assigned": [variable("y", false, false)], "unevaluated": [], "removed": [], "version": 2
            }}),
            cx,
        );
        assert_eq!(harness.row_names(cx), ["acf", "lm"]);

        // Detaching the package shown goes back to the global environment.
        harness.session.update(cx, |_, cx| {
            cx.emit(SessionEvent::ExecutionFinished {
                request_id: "detach".to_string(),
                succeeded: true,
            })
        });
        cx.run_until_parked();
        harness.answer_search_path(&[], cx);
        assert_eq!(harness.row_names(cx), ["x", "y"]);
        assert_eq!(
            harness
                .panel
                .read_with(cx, |panel, _| panel.attached_packages.clone()),
            ["package:base"]
        );
    }

    #[gpui::test]
    async fn test_only_the_last_chosen_environment_is_shown(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        harness.send_variables_event(
            json!({ "method": "refresh", "params": { "variables": [], "length": 0, "version": 1 }}),
            cx,
        );
        harness.answer_search_path(&["package:stats"], cx);

        harness.panel.update(cx, |panel, cx| {
            panel.select_environment(Some("package:stats".to_string()), cx);
            panel.select_environment(Some("package:base".to_string()), cx);
        });
        harness.answer(
            json!({ "result": [function_record("lm")], "output": "" }),
            cx,
        );
        assert_eq!(
            harness
                .panel
                .read_with(cx, |panel, cx| panel.placeholder(cx)),
            Some("Loading…")
        );
        // R's empty list arrives as null.
        harness.answer(json!({ "result": null, "output": "" }), cx);
        assert_eq!(
            harness
                .panel
                .read_with(cx, |panel, cx| panel.placeholder(cx)),
            Some("This environment is empty.")
        );

        harness
            .panel
            .update(cx, |panel, cx| panel.select_environment(None, cx));
        assert_eq!(
            harness
                .panel
                .read_with(cx, |panel, cx| panel.placeholder(cx)),
            Some("Variables you create in R appear here.")
        );
    }
}
