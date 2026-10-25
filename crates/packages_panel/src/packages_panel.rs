//! The Packages pane: the R session's installed packages, attached or not,
//! with updates, and search across CRAN, Bioconductor and R-universe from an
//! offline index. What the pane does runs in the console, as in RStudio.

mod commands;
mod rows;

use anyhow::Result;
use collections::HashMap;
use editor::Editor;
use gpui::{
    Action, App, AsyncWindowContext, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    Pixels, PromptLevel, Subscription, Task, WeakEntity, Window, actions, px, uniform_list,
};
use package_index::{IndexStatus, IndexedPackage, PackageIndex, PackageSource};
use r_console::ConsolePanel;
use rows::{DetailState, Row, RowInputs, build_rows};
use runtime::comms::InstalledPackage;
use runtime::{RSession, SessionAdded, SessionEvent, SessionRegistry, SessionState};
use std::ops::Range;
use ui::{Button, ButtonStyle, Checkbox, IconButton, IconName, ToggleState, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

actions!(
    packages_panel,
    [
        /// Opens the Packages pane and moves focus to it.
        ToggleFocus,
        /// Lists installed packages again and checks for updates.
        RefreshPackages,
        /// Updates every package with a newer version available.
        UpdateAllPackages,
    ]
);

const PANEL_KEY: &str = "PackagesPanel";
const ROW_HEIGHT: Pixels = px(26.);
const SEARCH_RESULTS: usize = 50;

pub fn init(cx: &mut App) {
    package_index::init(cx);
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<PackagesPanel>(window, cx);
        });
    })
    .detach();
}

pub struct PackagesPanel {
    workspace: WeakEntity<Workspace>,
    workspace_id: EntityId,
    session: Option<Entity<RSession>>,
    installed: Vec<InstalledPackage>,
    /// Latest versions of installed packages that have a newer one.
    outdated: HashMap<String, String>,
    checking_updates: bool,
    /// Whether the session's library is an renv project library.
    renv: bool,
    filter: Entity<Editor>,
    results: Vec<IndexedPackage>,
    searching: bool,
    expanded: Option<String>,
    details: HashMap<String, DetailState>,
    rows: Vec<Row>,
    /// The last failure, shown under the toolbar.
    error: Option<String>,
    /// Whether the pane is the one shown in its dock.
    active: bool,
    /// The list may be out of date: R ran code while the pane was hidden.
    stale: bool,
    /// Check for updates after the next execution, as after updating.
    recheck_updates: bool,
    focus_handle: FocusHandle,
    position: DockPosition,
    _search_task: Option<Task<()>>,
    _refresh_task: Option<Task<()>>,
    _updates_task: Option<Task<()>>,
    _session_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl PackagesPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            cx.new(|cx| Self::new(workspace, window, cx))
        })
    }

    pub fn new(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let workspace_handle = workspace.weak_handle();
        let workspace_id = workspace_handle.entity_id();
        let filter = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter or search packages", window, cx);
            editor
        });
        let mut subscriptions = vec![cx.subscribe(&filter, |this, _, event, cx| {
            if let editor::EditorEvent::BufferEdited = event {
                this.search(cx);
            }
        })];
        if let Some(index) = PackageIndex::global(cx) {
            // Search again once the index loads or downloads.
            subscriptions.push(cx.observe(&index, |this, _, cx| this.search(cx)));
        }
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
            workspace: workspace_handle,
            workspace_id,
            session: None,
            installed: Vec::new(),
            outdated: HashMap::default(),
            checking_updates: false,
            renv: false,
            filter,
            results: Vec::new(),
            searching: false,
            expanded: None,
            details: HashMap::default(),
            rows: Vec::new(),
            error: None,
            active: false,
            stale: true,
            recheck_updates: true,
            focus_handle: cx.focus_handle(),
            position: DockPosition::Right,
            _search_task: None,
            _refresh_task: None,
            _updates_task: None,
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
        self.forget_session(cx);
        self.refresh_if_shown(cx);
    }

    fn is_running(&self, cx: &App) -> bool {
        self.session.as_ref().is_some_and(|session| {
            matches!(
                session.read(cx).state(),
                SessionState::Idle | SessionState::Busy
            )
        })
    }

    fn is_idle(&self, cx: &App) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| *session.read(cx).state() == SessionState::Idle)
    }

    fn handle_session_event(&mut self, event: &SessionEvent, cx: &mut Context<Self>) {
        match event {
            SessionEvent::StateChanged => {
                if !self.is_running(cx) {
                    self.forget_session(cx);
                }
                // Lists once R has started, and once a run of executions is over.
                if self.is_idle(cx) && self.stale {
                    self.refresh_if_shown(cx);
                }
                cx.notify();
            }
            // Code may have attached, installed or removed packages.
            SessionEvent::ExecutionFinished { .. } => {
                self.stale = true;
                if self.is_idle(cx) {
                    self.refresh_if_shown(cx);
                }
            }
            _ => {}
        }
    }

    /// A new or stopped R has its own packages.
    fn forget_session(&mut self, cx: &mut Context<Self>) {
        self.installed.clear();
        self.outdated.clear();
        self.details.clear();
        self.expanded = None;
        self.renv = false;
        self.stale = true;
        self.recheck_updates = true;
        self.rebuild_rows(cx);
    }

    fn refresh_if_shown(&mut self, cx: &mut Context<Self>) {
        if self.active && self.stale && self.is_running(cx) {
            self.refresh(cx);
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        self.stale = false;
        let (installed, renv) = session.update(cx, |session, cx| {
            (
                session.installed_packages(cx),
                session.evaluate_code("nzchar(Sys.getenv(\"RENV_PROJECT\"))", cx),
            )
        });
        self._refresh_task = Some(cx.spawn(async move |this, cx| {
            let installed = installed.await;
            let renv = renv.await;
            this.update(cx, |this, cx| {
                match installed {
                    Ok(installed) => {
                        this.installed = installed;
                        this.error = None;
                    }
                    Err(error) => this.error = Some(format!("Couldn't list packages: {error:#}")),
                }
                if let Some(renv) = renv.log_err() {
                    this.renv = renv.result.as_bool() == Some(true);
                }
                // Details may have changed with a new version.
                this.details.clear();
                if let Some(expanded) = this.expanded.clone() {
                    this.fetch_detail(expanded, cx);
                }
                if this.recheck_updates {
                    this.check_updates(cx);
                }
                this.rebuild_rows(cx);
            })
            .log_err();
        }));
    }

    /// Asks the session's repositories for newer versions, which needs the network.
    fn check_updates(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        self.recheck_updates = false;
        self.checking_updates = true;
        let outdated = session.update(cx, |session, cx| session.outdated_packages(cx));
        self._updates_task = Some(cx.spawn(async move |this, cx| {
            let outdated = outdated.await;
            this.update(cx, |this, cx| {
                this.checking_updates = false;
                match outdated {
                    Ok(outdated) => {
                        this.outdated = outdated
                            .into_iter()
                            .map(|package| (package.name, package.latest_version))
                            .collect();
                    }
                    Err(error) => {
                        this.error = Some(format!("Couldn't check for updates: {error:#}"))
                    }
                }
                this.rebuild_rows(cx);
            })
            .log_err();
        }));
        cx.notify();
    }

    fn search(&mut self, cx: &mut Context<Self>) {
        let query = self.filter.read(cx).text(cx);
        let index = PackageIndex::global(cx);
        match index {
            Some(index) if !query.trim().is_empty() => {
                self.searching = true;
                let search = index.read(cx).search(&query, SEARCH_RESULTS, cx);
                self._search_task = Some(cx.spawn(async move |this, cx| {
                    let results = search.await;
                    this.update(cx, |this, cx| {
                        // A newer keystroke has its own search.
                        if this.filter.read(cx).text(cx) == query {
                            this.results = results;
                            this.searching = false;
                            this.rebuild_rows(cx);
                        }
                    })
                    .log_err();
                }));
            }
            _ => {
                self.results.clear();
                self.searching = false;
                self._search_task = None;
            }
        }
        self.rebuild_rows(cx);
    }

    fn rebuild_rows(&mut self, cx: &mut Context<Self>) {
        let filter = self.filter.read(cx).text(cx);
        self.rows = build_rows(RowInputs {
            installed: &self.installed,
            outdated: &self.outdated,
            filter: &filter,
            results: &self.results,
            searching: self.searching,
            expanded: self.expanded.as_deref(),
            details: &self.details,
        });
        cx.notify();
    }

    fn toggle_expanded(&mut self, name: String, cx: &mut Context<Self>) {
        if self.expanded.as_deref() == Some(name.as_str()) {
            self.expanded = None;
        } else {
            if !self.details.contains_key(&name) {
                self.fetch_detail(name.clone(), cx);
            }
            self.expanded = Some(name);
        }
        self.rebuild_rows(cx);
    }

    fn fetch_detail(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        self.details.insert(name.clone(), DetailState::Loading);
        let detail = session.update(cx, |session, cx| session.package_detail(&name, cx));
        cx.spawn(async move |this, cx| {
            let detail = detail.await;
            this.update(cx, |this, cx| {
                let state = match detail {
                    Ok(Some(detail)) => DetailState::Loaded(detail),
                    Ok(None) => DetailState::Failed("No longer installed.".to_string()),
                    Err(error) => DetailState::Failed(format!("{error:#}")),
                };
                this.details.insert(name, state);
                this.rebuild_rows(cx);
            })
            .log_err();
        })
        .detach();
    }

    /// Runs `code` in the console, showing it and its output there.
    fn run_in_console(&mut self, code: String, window: &mut Window, cx: &mut Context<Self>) {
        let console = self
            .workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).panel::<ConsolePanel>(cx));
        match console {
            Some(console) => {
                console.update(cx, |console, cx| console.run_from_editor(&code, cx));
                let workspace = self.workspace.clone();
                // Opening the console updates the docks, which can't happen mid-update.
                window.defer(cx, move |window, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.open_panel::<ConsolePanel>(window, cx)
                        })
                        .log_err();
                });
            }
            None => {
                if let Some(session) = self.session.clone()
                    && let Err(error) = session.update(cx, |session, cx| session.execute(code, cx))
                {
                    self.error = Some(format!("{error:#}"));
                    cx.notify();
                }
            }
        }
    }

    fn toggle_attached(
        &mut self,
        package: &InstalledPackage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let code = if package.attached {
            commands::detach(&package.name)
        } else {
            commands::attach(&package.name)
        };
        self.run_in_console(code, window, cx);
    }

    fn install(&mut self, package: &IndexedPackage, window: &mut Window, cx: &mut Context<Self>) {
        self.run_in_console(commands::install(package, self.renv), window, cx);
    }

    fn update(&mut self, names: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        if names.is_empty() {
            return;
        }
        self.recheck_updates = true;
        self.run_in_console(commands::update(&names, self.renv), window, cx);
    }

    fn update_all(&mut self, _: &UpdateAllPackages, window: &mut Window, cx: &mut Context<Self>) {
        let mut names: Vec<String> = self.outdated.keys().cloned().collect();
        names.sort();
        self.update(names, window, cx);
    }

    fn refresh_packages(&mut self, _: &RefreshPackages, _: &mut Window, cx: &mut Context<Self>) {
        self.recheck_updates = true;
        self.refresh(cx);
        if let Some(index) = PackageIndex::global(cx)
            && matches!(index.read(cx).status(), IndexStatus::Failed(_))
        {
            index.update(cx, |index, cx| index.refresh(cx));
        }
    }

    fn remove(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Remove {name}?"),
            Some("This uninstalls the package from the library."),
            &["Remove", "Cancel"],
            cx,
        );
        let code = commands::remove(&name, self.renv);
        cx.spawn_in(window, async move |this, cx| {
            if answer.await.ok() == Some(0) {
                this.update_in(cx, |this, window, cx| this.run_in_console(code, window, cx))
                    .log_err();
            }
        })
        .detach();
    }

    fn placeholder(&self, cx: &App) -> Option<&'static str> {
        let Some(session) = &self.session else {
            return Some("R isn't running in this window.");
        };
        match session.read(cx).state() {
            SessionState::Idle | SessionState::Busy => {
                if self.installed.is_empty() && self.rows.is_empty() {
                    Some("Listing packages…")
                } else {
                    None
                }
            }
            SessionState::Starting | SessionState::Restarting => Some("Starting R…"),
            SessionState::Restricted => Some("R starts once you trust this folder."),
            SessionState::Unavailable(_) | SessionState::Exited(_) => Some("R isn't running."),
        }
    }

    /// A note about the search index, when the filter is searching it.
    fn index_note(&self, cx: &App) -> Option<(String, Color)> {
        if self.filter.read(cx).text(cx).trim().is_empty() {
            return None;
        }
        let index = PackageIndex::global(cx)?;
        let index = index.read(cx);
        match index.status() {
            IndexStatus::Failed(error) if index.is_empty() => Some((
                format!("Couldn't download the package index: {error}"),
                Color::Error,
            )),
            IndexStatus::Loading | IndexStatus::Downloading if index.is_empty() => Some((
                "Downloading the package index, about 15 MB, once…".to_string(),
                Color::Muted,
            )),
            _ => None,
        }
    }

    fn render_row(&self, index: usize, row: &Row, cx: &mut Context<Self>) -> AnyElement {
        let muted = |text: String| {
            Label::new(text)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .truncate()
        };
        let line = || {
            h_flex()
                .id(("package-row", index))
                .group("package-row")
                .relative()
                .h(ROW_HEIGHT)
                .w_full()
                .px_2()
                .gap_2()
        };
        match row {
            Row::Heading(text) => line()
                .child(
                    Label::new(text.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .weight(gpui::FontWeight::MEDIUM),
                )
                .into_any_element(),
            Row::Note(text) => line().pl_8().child(muted(text.clone())).into_any_element(),
            Row::Detail { label, value, url } => {
                let value_label = muted(value.clone());
                line()
                    .pl_8()
                    .when(!label.is_empty(), |this| {
                        this.child(
                            div()
                                .w(px(80.))
                                .flex_none()
                                .child(Label::new(*label).size(LabelSize::Small)),
                        )
                    })
                    .child(div().flex_1().min_w_0().child(value_label))
                    .when_some(url.clone(), |this, url| {
                        this.cursor_pointer()
                            .tooltip(Tooltip::text("Open Website"))
                            .on_click(move |_, _, cx| cx.open_url(&url))
                    })
                    .into_any_element()
            }
            Row::Installed {
                package,
                latest,
                expanded,
            } => {
                let name = package.name.clone();
                let attached = package.attached;
                let toggle_package = package.clone();
                let mut actions = h_flex().gap_1();
                if latest.is_some() {
                    let update_name = name.clone();
                    actions = actions.child(
                        IconButton::new(("update-package", index), IconName::ArrowCircle)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Update"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.update(vec![update_name.clone()], window, cx)
                            })),
                    );
                }
                let remove_name = name.clone();
                actions = actions.child(
                    IconButton::new(("remove-package", index), IconName::Trash)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Remove"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.remove(remove_name.clone(), window, cx)
                        })),
                );
                let expand_name = name.clone();
                line()
                    .cursor_pointer()
                    .when(*expanded, |this| {
                        this.bg(cx.theme().colors().ghost_element_selected)
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_expanded(expand_name.clone(), cx)
                    }))
                    .child(
                        Checkbox::new(("attached", index), ToggleState::from(attached))
                            .tooltip(Tooltip::text(if attached {
                                "Attached: click to detach"
                            } else {
                                "Click to attach with library()"
                            }))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.toggle_attached(&toggle_package, window, cx)
                            })),
                    )
                    .child(
                        h_flex()
                            .w(relative(0.35))
                            .min_w_0()
                            .gap_1()
                            .child(Label::new(name).size(LabelSize::Small).truncate()),
                    )
                    .child(
                        h_flex()
                            .w(px(110.))
                            .flex_none()
                            .gap_1()
                            .child(muted(package.version.clone()))
                            .children(latest.clone().map(|latest| {
                                Label::new(format!("→ {latest}"))
                                    .size(LabelSize::Small)
                                    .color(Color::Accent)
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(muted(package.description.clone())),
                    )
                    .child(
                        h_flex()
                            .absolute()
                            .right_1()
                            .h_full()
                            .pl_1()
                            .bg(cx.theme().colors().panel_background)
                            .visible_on_hover("package-row")
                            .child(actions),
                    )
                    .into_any_element()
            }
            Row::Available(package) => {
                let source = match package.source() {
                    PackageSource::Cran => "CRAN".to_string(),
                    PackageSource::Bioconductor => "Bioconductor".to_string(),
                    PackageSource::Universe(universe) => format!("{universe}.r-universe.dev"),
                };
                let install_package = package.clone();
                line()
                    .child(div().w(px(16.)).flex_none())
                    .child(
                        div().w(relative(0.35)).min_w_0().child(
                            Label::new(package.name.clone())
                                .size(LabelSize::Small)
                                .truncate(),
                        ),
                    )
                    .child(div().w(px(110.)).flex_none().child(muted(source)))
                    .child(div().flex_1().min_w_0().child(muted(package.title.clone())))
                    .child(
                        h_flex()
                            .absolute()
                            .right_1()
                            .h_full()
                            .pl_1()
                            .bg(cx.theme().colors().panel_background)
                            .visible_on_hover("package-row")
                            .child(
                                Button::new(("install-package", index), "Install")
                                    .style(ButtonStyle::Filled)
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.install(&install_package, window, cx)
                                    })),
                            ),
                    )
                    .into_any_element()
            }
        }
    }
}

impl Render for PackagesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let placeholder = self.placeholder(cx);
        let row_count = self.rows.len();
        let outdated = self.outdated.len();
        let note = self.index_note(cx);
        v_flex()
            .key_context("PackagesPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::update_all))
            .on_action(cx.listener(Self::refresh_packages))
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(div().flex_1().child(self.filter.clone()))
                    .when(self.renv, |this| {
                        this.child(
                            div()
                                .id("renv")
                                .tooltip(Tooltip::text(
                                    "This project uses renv: packages install into its library.",
                                ))
                                .child(
                                    Label::new("renv")
                                        .size(LabelSize::Small)
                                        .color(Color::Accent),
                                ),
                        )
                    })
                    .when(outdated > 0, |this| {
                        this.child(
                            Button::new("update-all", format!("Update All ({outdated})"))
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.update_all(&UpdateAllPackages, window, cx)
                                })),
                        )
                    })
                    .child(
                        IconButton::new("refresh-packages", IconName::RotateCw)
                            .icon_size(IconSize::Small)
                            .disabled(!self.is_running(cx))
                            .tooltip(Tooltip::text(if self.checking_updates {
                                "Checking for updates…"
                            } else {
                                "Refresh and Check for Updates"
                            }))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.refresh_packages(&RefreshPackages, window, cx)
                            })),
                    ),
            )
            .children(self.error.clone().map(|error| {
                div().px_2().py_1().child(
                    Label::new(error)
                        .size(LabelSize::Small)
                        .color(Color::Error)
                        .single_line()
                        .truncate(),
                )
            }))
            .children(note.map(|(text, color)| {
                div()
                    .px_2()
                    .py_1()
                    .child(Label::new(text).size(LabelSize::Small).color(color))
            }))
            .child(match placeholder {
                Some(text) => div()
                    .flex_1()
                    .p_4()
                    .child(Label::new(text).size(LabelSize::Small).color(Color::Muted))
                    .into_any_element(),
                None => uniform_list(
                    "packages-rows",
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

impl Focusable for PackagesPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for PackagesPanel {}

impl Panel for PackagesPanel {
    fn persistent_name() -> &'static str {
        "Packages"
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
        Some(IconName::Blocks)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Packages")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn starts_open(&self, _: &Window, _: &App) -> bool {
        false
    }

    fn activation_priority(&self) -> u32 {
        11
    }

    /// Listing only happens while the pane is shown, so a hidden pane costs R nothing.
    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.active = active;
        self.refresh_if_shown(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::channel::mpsc;
    use gpui::{TestAppContext, VisualTestContext};
    use http_client::FakeHttpClient;
    use jupyter_protocol::{
        CommId, CommMsg, ExecuteReply, JupyterMessage, JupyterMessageContent, KernelInfoReply,
        Status,
    };
    use languages::r::{RInstallationSource, RToolchainData};
    use project::{FakeFs, Project};
    use serde_json::{Value, json};
    use settings::SettingsStore;
    use std::path::PathBuf;
    use workspace::MultiWorkspace;

    struct Harness {
        panel: Entity<PackagesPanel>,
        session: Entity<RSession>,
        requests: mpsc::Receiver<JupyterMessage>,
        ui_comm: CommId,
    }

    fn indexed(name: &str, universe: &str, on_cran: bool) -> IndexedPackage {
        IndexedPackage {
            name: name.into(),
            title: format!("About {name}"),
            description: String::new(),
            universe: universe.into(),
            used_by: 10,
            cran_version: on_cran.then(|| "1.0".into()),
        }
    }

    fn installed(name: &str, attached: bool) -> Value {
        json!({ "id": format!("{name}-1.0"), "name": name, "displayName": name, "version": "1.0",
                "attached": attached, "description": format!("About {name}.") })
    }

    impl Harness {
        async fn new(cx: &mut TestAppContext) -> (Self, &mut VisualTestContext) {
            cx.update(|cx| {
                let settings = SettingsStore::test(cx);
                cx.set_global(settings);
                theme_settings::init(theme::LoadThemes::JustBase, cx);
                editor::init(cx);
                let index = cx.new(|_| {
                    PackageIndex::new_for_test(
                        vec![
                            indexed("ggplot2", "tidyverse", true),
                            indexed("ggplotify", "cran", true),
                            indexed("ggtree", "bioc", false),
                        ],
                        FakeHttpClient::with_404_response(),
                    )
                });
                PackageIndex::set_global(index, cx);
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
            let ui_comm = match requests.try_recv().unwrap().content {
                JupyterMessageContent::CommOpen(open) => open.comm_id,
                other => panic!("expected the UI comm, got {}", other.message_type()),
            };
            while requests.try_recv().is_ok() {}
            let panel = workspace.update_in(cx, |workspace, window, cx| {
                let panel = cx.new(|cx| {
                    let mut panel = PackagesPanel::new(workspace, window, cx);
                    panel.set_session(session.clone(), window, cx);
                    panel
                });
                workspace.add_panel(panel.clone(), window, cx);
                workspace.open_panel::<PackagesPanel>(window, cx);
                panel
            });
            let harness = Self {
                panel,
                session,
                requests,
                ui_comm,
            };
            let reply: KernelInfoReply = serde_json::from_value(json!({
                "status": "ok", "protocol_version": "5.3", "implementation": "ark",
                "implementation_version": "0.1.252",
                "language_info": { "name": "R", "version": "4.5.3" },
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

        /// The R functions called and code run since the last call, in order.
        fn requests(&mut self) -> Vec<(String, JupyterMessage)> {
            std::iter::from_fn(|| self.requests.try_recv().ok())
                .map(|message| {
                    let label = match &message.content {
                        JupyterMessageContent::CommMsg(comm)
                            if comm.data["method"] == "call_method" =>
                        {
                            format!(
                                "{}()",
                                comm.data["params"]["method"].as_str().unwrap_or_default()
                            )
                        }
                        JupyterMessageContent::CommMsg(comm) => {
                            format!(
                                "{}: {}",
                                comm.data["method"].as_str().unwrap_or_default(),
                                comm.data["params"]["code"].as_str().unwrap_or_default()
                            )
                        }
                        JupyterMessageContent::ExecuteRequest(request) => {
                            format!("run: {}", request.code)
                        }
                        other => other.message_type().to_string(),
                    };
                    (label, message)
                })
                .collect()
        }

        fn labels(&mut self) -> Vec<String> {
            self.requests()
                .into_iter()
                .map(|(label, _)| label)
                .collect()
        }

        /// Answers the refresh: the installed list, whether renv is on, and
        /// (when asked) the outdated packages.
        fn answer_refresh(
            &mut self,
            installed: Value,
            renv: bool,
            outdated: Option<Value>,
            cx: &mut VisualTestContext,
        ) {
            let requests = self.requests();
            let labels: Vec<&str> = requests.iter().map(|(label, _)| label.as_str()).collect();
            assert_eq!(
                labels,
                [
                    "pkg_list()",
                    "evaluate_code: nzchar(Sys.getenv(\"RENV_PROJECT\"))"
                ]
            );
            self.reply(
                &requests[0].1,
                json!({ "method": "call_method", "result": installed }),
                cx,
            );
            self.reply(
                &requests[1].1,
                json!({ "method": "evaluate_code", "result": { "result": renv, "output": "" } }),
                cx,
            );
            let requests = self.requests();
            match outdated {
                Some(outdated) => {
                    assert_eq!(
                        requests.len(),
                        1,
                        "{:?}",
                        requests.iter().map(|r| &r.0).collect::<Vec<_>>()
                    );
                    assert_eq!(requests[0].0, "pkg_outdated()");
                    self.reply(
                        &requests[0].1,
                        json!({ "method": "call_method", "result": outdated }),
                        cx,
                    );
                }
                None => assert!(
                    requests.is_empty(),
                    "{:?}",
                    requests.iter().map(|r| &r.0).collect::<Vec<_>>()
                ),
            }
        }

        fn reply(&self, request: &JupyterMessage, data: Value, cx: &mut VisualTestContext) {
            let Value::Object(data) = data else {
                panic!("reply data must be an object");
            };
            self.receive(
                JupyterMessage::new(
                    CommMsg {
                        comm_id: self.ui_comm.clone(),
                        data,
                    },
                    Some(request),
                ),
                cx,
            );
        }

        /// R running `code` and finishing.
        fn run(&mut self, code: &str, cx: &mut VisualTestContext) {
            self.session
                .update(cx, |session, cx| session.execute(code.to_string(), cx))
                .unwrap();
            let request = self.requests.try_recv().unwrap();
            self.finish(&request, cx);
        }

        fn finish(&self, request: &JupyterMessage, cx: &mut VisualTestContext) {
            self.receive(
                JupyterMessage::new(ExecuteReply::default(), Some(request)),
                cx,
            );
            self.receive(JupyterMessage::new(Status::idle(), Some(request)), cx);
        }

        fn rows(&self, cx: &mut VisualTestContext) -> Vec<String> {
            self.panel.read_with(cx, |panel, _| {
                panel
                    .rows
                    .iter()
                    .map(|row| match row {
                        Row::Heading(text) => format!("# {text}"),
                        Row::Installed {
                            package, latest, ..
                        } => format!(
                            "[{}] {}{}",
                            if package.attached { "x" } else { " " },
                            package.name,
                            latest
                                .as_ref()
                                .map(|latest| format!(" -> {latest}"))
                                .unwrap_or_default()
                        ),
                        Row::Detail { label, value, .. } => format!("  {label}: {value}"),
                        Row::Available(package) => format!("+ {}", package.name),
                        Row::Note(text) => format!("({text})"),
                    })
                    .collect()
            })
        }

        fn set_filter(&self, text: &str, cx: &mut VisualTestContext) {
            self.panel.update_in(cx, |panel, window, cx| {
                panel
                    .filter
                    .update(cx, |editor, cx| editor.set_text(text, window, cx))
            });
            cx.run_until_parked();
        }
    }

    #[gpui::test]
    async fn test_lists_once_shown_and_after_code_runs(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        harness.answer_refresh(
            json!([installed("stats", true), installed("cli", false)]),
            false,
            Some(json!([{ "name": "cli", "latestVersion": "3.7.0" }])),
            cx,
        );
        assert_eq!(harness.rows(cx), ["[ ] cli -> 3.7.0", "[x] stats"]);

        // Code that runs may attach or install packages, so the list is fetched again.
        harness.run("library(cli)", cx);
        harness.answer_refresh(
            json!([installed("stats", true), installed("cli", true)]),
            false,
            None,
            cx,
        );
        assert_eq!(harness.rows(cx), ["[x] cli -> 3.7.0", "[x] stats"]);

        // While the pane is hidden, nothing is asked of R until it's shown again.
        harness
            .panel
            .update_in(cx, |panel, window, cx| panel.set_active(false, window, cx));
        harness.run("x <- 1", cx);
        assert!(harness.labels().is_empty());
        harness
            .panel
            .update_in(cx, |panel, window, cx| panel.set_active(true, window, cx));
        cx.run_until_parked();
        harness.answer_refresh(json!([installed("stats", true)]), false, None, cx);
        assert_eq!(harness.rows(cx), ["[x] stats"]);

        // Expanding a package shows its details.
        harness
            .panel
            .update(cx, |panel, cx| panel.toggle_expanded("stats".into(), cx));
        cx.run_until_parked();
        let requests = harness.requests();
        assert_eq!(requests[0].0, "pkg_detail()");
        harness.reply(
            &requests[0].1,
            json!({ "method": "call_method", "result": { "name": "stats", "title": "The R Stats Package", "license": "Part of R 4.5.3" } }),
            cx,
        );
        assert_eq!(
            harness.rows(cx),
            [
                "[x] stats",
                "  Title: The R Stats Package",
                "  Description: About stats.",
                "  License: Part of R 4.5.3",
            ]
        );

        // When R stops, its packages are gone from the pane.
        harness
            .session
            .update(cx, |session, cx| session.stop(cx))
            .detach();
        cx.run_until_parked();
        assert!(harness.rows(cx).is_empty());
    }

    #[gpui::test]
    async fn test_actions_run_in_r(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        harness.answer_refresh(
            json!([
                installed("stats", true),
                installed("cli", false),
                installed("ggplot2", false)
            ]),
            false,
            Some(json!([
                { "name": "cli", "latestVersion": "3.7.0" },
                { "name": "ggplot2", "latestVersion": "4.1.0" },
            ])),
            cx,
        );

        // Search lists what isn't installed, from the index.
        harness.set_filter("gg", cx);
        assert_eq!(
            harness.rows(cx),
            [
                "# Installed (1)",
                "[ ] ggplot2 -> 4.1.0",
                "# Available",
                "+ ggtree",
                "+ ggplotify"
            ]
        );

        let act =
            |harness: &mut Harness,
             action: &dyn Fn(&mut PackagesPanel, &mut Window, &mut Context<PackagesPanel>),
             cx: &mut VisualTestContext| {
                harness
                    .panel
                    .update_in(cx, |panel, window, cx| action(panel, window, cx));
                cx.run_until_parked();
                harness.labels()
            };
        let cli = harness
            .panel
            .read_with(cx, |panel, _| panel.installed[1].clone());
        let stats = harness
            .panel
            .read_with(cx, |panel, _| panel.installed[0].clone());
        assert_eq!(
            act(
                &mut harness,
                &|panel, window, cx| panel.toggle_attached(&cli, window, cx),
                cx
            ),
            ["run: library(cli)"]
        );
        assert_eq!(
            act(
                &mut harness,
                &|panel, window, cx| panel.toggle_attached(&stats, window, cx),
                cx
            ),
            ["run: detach(\"package:stats\", unload = TRUE)"]
        );
        let ggtree = indexed("ggtree", "bioc", false);
        assert_eq!(
            act(
                &mut harness,
                &|panel, window, cx| panel.install(&ggtree, window, cx),
                cx
            ),
            [
                "run: if (!requireNamespace(\"BiocManager\", quietly = TRUE)) install.packages(\"BiocManager\")\nBiocManager::install(\"ggtree\")"
            ]
        );
        assert_eq!(
            act(
                &mut harness,
                &|panel, window, cx| panel.update_all(&UpdateAllPackages, window, cx),
                cx
            ),
            ["run: install.packages(c(\"cli\", \"ggplot2\"))"]
        );
        // Removing asks first.
        assert!(
            act(
                &mut harness,
                &|panel, window, cx| panel.remove("cli".into(), window, cx),
                cx
            )
            .is_empty()
        );
        cx.simulate_prompt_answer("Remove");
        cx.run_until_parked();
        assert_eq!(harness.labels(), ["run: remove.packages(\"cli\")"]);
    }

    #[gpui::test]
    async fn test_renv_projects_use_renv(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        harness.answer_refresh(json!([installed("cli", false)]), true, Some(json!({})), cx);
        assert!(harness.panel.read_with(cx, |panel, _| panel.renv));
        let ggplotify = indexed("ggplotify", "cran", true);
        harness.panel.update_in(cx, |panel, window, cx| {
            panel.install(&ggplotify, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(harness.labels(), ["run: renv::install(\"ggplotify\")"]);
        harness.panel.update_in(cx, |panel, window, cx| {
            panel.remove("cli".into(), window, cx)
        });
        cx.run_until_parked();
        cx.simulate_prompt_answer("Remove");
        cx.run_until_parked();
        assert_eq!(harness.labels(), ["run: renv::remove(\"cli\")"]);
    }
}
