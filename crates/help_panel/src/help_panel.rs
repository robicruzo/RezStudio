//! The Help pane: R's help pages, served by Ark's help proxy and shown in a
//! web view. `?topic`, links in help pages, the topic box and F1 all end in
//! a page here.

mod page_style;

use anyhow::Result;
use editor::Editor;
use gpui::{
    Action, App, AsyncWindowContext, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    Pixels, Subscription, Task, WeakEntity, Window, actions, px,
};
use r_console::ConsolePanel;
use r_console::completions::{cursor_position, help_server, request_help_topic};
use runtime::{RSession, SessionAdded, SessionEvent, SessionRegistry, SessionState};
use ui::{IconButton, IconName, Tooltip, prelude::*};
use util::ResultExt as _;
use web_view::{WebView, WebViewEvent, watch_cover, web_view};
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

actions!(
    help_panel,
    [
        /// Opens the Help pane and moves focus to its topic box.
        ToggleFocus,
        /// Shows R's help for the function or object under the cursor, in R
        /// files, R Markdown and Quarto chunks, and the console.
        ShowHelpAtCursor,
    ]
);

const PANEL_KEY: &str = "HelpPanel";

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace
            .register_action(|workspace, _: &ToggleFocus, window, cx| {
                workspace.toggle_panel_focus::<HelpPanel>(window, cx);
            })
            .register_action(|_, _: &ShowHelpAtCursor, window, cx| {
                let workspace = cx.entity();
                // Finding Ark's server reads the workspace, which is being updated now.
                window.defer(cx, move |window, cx| {
                    show_help_at_cursor(&workspace, window, cx)
                });
            });
    })
    .detach();
}

/// Looks up the topic at the cursor of whichever R code has focus, and shows
/// its help in the workspace's Help pane.
fn show_help_at_cursor(workspace: &Entity<Workspace>, window: &mut Window, cx: &mut App) {
    let Some(panel) = workspace.read(cx).panel::<HelpPanel>(cx) else {
        return;
    };
    let session = panel.read(cx).session.clone();
    let Some(topic) = topic_at_cursor(workspace, session, window, cx) else {
        return;
    };
    panel.update(cx, |panel, cx| panel.show_topic(topic, window, cx));
}

fn topic_at_cursor(
    workspace: &Entity<Workspace>,
    session: Option<Entity<RSession>>,
    window: &mut Window,
    cx: &mut App,
) -> Option<Task<Result<Option<String>>>> {
    if let Some(console) = workspace.read(cx).panel::<ConsolePanel>(cx)
        && let Some(topic) =
            console.update(cx, |console, cx| console.help_topic_at_cursor(window, cx))
    {
        return Some(topic);
    }
    let editor = workspace.read(cx).active_item_as::<Editor>(cx)?;
    if !editor.focus_handle(cx).contains_focused(window, cx) {
        return None;
    }
    if let Some(topic) = r_markdown::help_topic_at_cursor(&editor, cx) {
        return Some(topic);
    }
    r_file_topic_at_cursor(workspace, session, &editor, cx)
}

/// For an R file, Ark already has the file open, as Zed keeps it in sync.
fn r_file_topic_at_cursor(
    workspace: &Entity<Workspace>,
    session: Option<Entity<RSession>>,
    editor: &Entity<Editor>,
    cx: &mut App,
) -> Option<Task<Result<Option<String>>>> {
    let buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
    let buffer = buffer.read(cx);
    if buffer.language()?.name().as_ref() != runtime::R_LANGUAGE_NAME {
        return None;
    }
    let path = language::LocalFile::abs_path(project::File::from_dyn(buffer.file())?, cx);
    let Ok(uri) = lsp::Uri::from_file_path(&path) else {
        return Some(Task::ready(Err(anyhow::anyhow!(
            "invalid path {}",
            path.display()
        ))));
    };
    let Some(session) = session else {
        return Some(Task::ready(Err(anyhow::anyhow!("R isn't running."))));
    };
    let server = match help_server(workspace, &session, cx) {
        Ok(server) => server,
        Err(error) => return Some(Task::ready(Err(error))),
    };
    let (_, position) = cursor_position(editor, cx)?;
    Some(request_help_topic(server, uri, position, cx))
}

/// The pages shown, for back and forward.
#[derive(Default)]
struct History {
    pages: Vec<String>,
    index: usize,
}

impl History {
    fn current(&self) -> Option<&str> {
        self.pages.get(self.index).map(String::as_str)
    }

    /// Adds `url` after the current page, dropping the pages forward of it.
    fn visit(&mut self, url: String) {
        if self.current() == Some(url.as_str()) {
            return;
        }
        self.pages.truncate(self.index + 1);
        self.pages.push(url);
        self.index = self.pages.len().saturating_sub(1);
    }

    fn replace_current(&mut self, url: String) {
        match self.pages.get_mut(self.index) {
            Some(page) => *page = url,
            None => self.visit(url),
        }
    }

    fn can_go_back(&self) -> bool {
        self.index > 0
    }

    fn can_go_forward(&self) -> bool {
        self.index + 1 < self.pages.len()
    }

    fn back(&mut self) -> Option<String> {
        if !self.can_go_back() {
            return None;
        }
        self.index -= 1;
        self.current().map(str::to_string)
    }

    fn forward(&mut self) -> Option<String> {
        if !self.can_go_forward() {
            return None;
        }
        self.index += 1;
        self.current().map(str::to_string)
    }

    fn clear(&mut self) {
        self.pages.clear();
        self.index = 0;
    }
}

pub struct HelpPanel {
    workspace: WeakEntity<Workspace>,
    workspace_id: EntityId,
    session: Option<Entity<RSession>>,
    web_view: Entity<WebView>,
    history: History,
    /// Whether the page loading now is one the pane asked for, rather than a
    /// link followed in the page. Where it ends up, after any redirect,
    /// becomes the current page instead of a new one.
    loading_own_page: bool,
    topic: Entity<Editor>,
    /// Why the last lookup showed nothing, under the toolbar.
    notice: Option<String>,
    /// Whether the pane is the one shown in its dock.
    active: bool,
    /// GPUI's focus while the page has the keyboard, so the page can give
    /// it back when focus moves on.
    page_focus: FocusHandle,
    position: DockPosition,
    _lookup: Option<Task<()>>,
    _session_subscription: Option<Subscription>,
    _cover_subscriptions: Vec<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl HelpPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            cx.new(|cx| {
                let web_view = cx.new(WebView::new);
                Self::new(workspace, web_view, window, cx)
            })
        })
    }

    pub fn new(
        workspace: &mut Workspace,
        web_view: Entity<WebView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let workspace_handle = workspace.weak_handle();
        let workspace_id = workspace_handle.entity_id();
        let topic = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search help topics", window, cx);
            editor
        });
        let page_focus = cx.focus_handle();
        let mut subscriptions = vec![
            cx.subscribe_in(
                &web_view,
                window,
                |this, _, event, window, cx| match event {
                    WebViewEvent::Navigated(url) => this.navigated(url.clone(), cx),
                    WebViewEvent::Clicked => window.focus(&this.page_focus, cx),
                    WebViewEvent::TitleChanged(_) => {}
                },
            ),
            cx.on_blur(&page_focus, window, |this, _, cx| {
                this.web_view.read(cx).release_keyboard();
            }),
        ];
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
        // The workspace is being updated now, and watching it reads it.
        cx.defer_in(window, |this, window, cx| {
            let Some(workspace) = this.workspace.upgrade() else {
                return;
            };
            this._cover_subscriptions = watch_cover(
                &workspace,
                cx.entity_id(),
                window,
                cx,
                |this, covered, cx| {
                    this.web_view
                        .update(cx, |web_view, cx| web_view.set_covered(covered, cx));
                },
            );
        });
        let mut this = Self {
            workspace: workspace_handle,
            workspace_id,
            session: None,
            web_view,
            history: History::default(),
            loading_own_page: false,
            topic,
            notice: None,
            active: false,
            page_focus,
            position: DockPosition::Right,
            _lookup: None,
            _session_subscription: None,
            _cover_subscriptions: Vec::new(),
            _subscriptions: subscriptions,
        };
        let existing = SessionRegistry::global(cx)
            .and_then(|registry| registry.read(cx).session(workspace_id).cloned());
        if let Some(session) = existing {
            this.set_session(session, window, cx);
        }
        this
    }

    /// Runs as the pane renders. Theme, setting and font size changes all
    /// redraw every window, cached panes included, but the font size
    /// adjustments of zooming in and out can't be observed directly.
    fn restyle(&mut self, cx: &mut Context<Self>) {
        let style = page_style::help_page_style(cx);
        self.web_view
            .update(cx, |web_view, cx| web_view.set_page_style(style, cx));
    }

    pub fn set_session(
        &mut self,
        session: Entity<RSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._session_subscription = Some(cx.subscribe_in(
            &session,
            window,
            |this, _, event, window, cx| this.handle_session_event(event, window, cx),
        ));
        self.session = Some(session);
        self.forget_pages(cx);
    }

    fn is_running(&self, cx: &App) -> bool {
        self.session.as_ref().is_some_and(|session| {
            matches!(
                session.read(cx).state(),
                SessionState::Idle | SessionState::Busy
            )
        })
    }

    fn handle_session_event(
        &mut self,
        event: &SessionEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SessionEvent::ShowHelp { url, .. } => self.show_page(url.clone(), window, cx),
            // The pages were served by the R that stopped.
            SessionEvent::StateChanged if !self.is_running(cx) => self.forget_pages(cx),
            SessionEvent::StateChanged => cx.notify(),
            _ => {}
        }
    }

    fn forget_pages(&mut self, cx: &mut Context<Self>) {
        self.history.clear();
        self.loading_own_page = false;
        self.update_shown(cx);
        cx.notify();
    }

    /// Shows a page from Ark's help proxy and brings the pane to the front.
    /// Ark's `focus` asks for no more: keyboard focus stays where it is, as
    /// it does in Positron, so typing at the console carries on.
    fn show_page(&mut self, url: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.web_view.read(cx).is_available() {
            cx.open_url(&url);
            return;
        }
        self.notice = None;
        self.history.visit(url.clone());
        self.load_page(url, cx);
        self.reveal(window, cx);
    }

    fn load_page(&mut self, url: String, cx: &mut Context<Self>) {
        self.loading_own_page = true;
        self.web_view
            .update(cx, |web_view, cx| web_view.load_url(url, cx));
        self.update_shown(cx);
        cx.notify();
    }

    fn navigated(&mut self, url: String, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.loading_own_page) {
            self.history.replace_current(url);
        } else {
            self.history.visit(url);
        }
        cx.notify();
    }

    fn reveal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        // Opening a panel updates the docks, which can't happen mid-update.
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.open_panel::<HelpPanel>(window, cx)
                })
                .log_err();
        });
    }

    fn update_shown(&mut self, cx: &mut Context<Self>) {
        let shown = self.active && self.history.current().is_some();
        self.web_view
            .update(cx, |web_view, cx| web_view.set_shown(shown, cx));
    }

    /// Shows help for the topic `topic` resolves to, saying so in the pane
    /// when R has none. A topic of `None` means there was nothing to look up.
    pub fn show_topic(
        &mut self,
        topic: Task<Result<Option<String>>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session = self.session.clone();
        self._lookup = Some(cx.spawn_in(window, async move |this, cx| {
            let result: Result<Option<(String, bool)>> = async {
                let Some(topic) = topic.await? else {
                    return Ok(None);
                };
                let session = session.ok_or_else(|| anyhow::anyhow!("R isn't running."))?;
                let found = session
                    .update(cx, |session, cx| session.show_help_topic(&topic, cx))
                    .await?;
                Ok(Some((topic, found)))
            }
            .await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(None) => return,
                    // The page arrives as a `ShowHelp`.
                    Ok(Some((_, true))) => this.notice = None,
                    Ok(Some((topic, false))) => {
                        this.notice = Some(format!("No help found for `{topic}`."))
                    }
                    Err(error) => this.notice = Some(format!("{error:#}")),
                }
                this.reveal(window, cx);
                cx.notify();
            })
            .log_err();
        }));
    }

    fn search(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let topic = self.topic.read(cx).text(cx).trim().to_string();
        if topic.is_empty() {
            return;
        }
        self.show_topic(Task::ready(Ok(Some(topic))), window, cx);
    }

    /// R's help index, which `help.start()` sends through Ark like any page.
    fn home(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let shown = session.update(cx, |session, cx| {
            session.evaluate_code("utils::help.start()", cx)
        });
        self._lookup = Some(cx.spawn_in(window, async move |this, cx| {
            if let Err(error) = shown.await {
                this.update(cx, |this, cx| {
                    this.notice = Some(format!("Couldn't open the help index: {error:#}"));
                    cx.notify();
                })
                .log_err();
            }
        }));
    }

    fn back(&mut self, cx: &mut Context<Self>) {
        if let Some(url) = self.history.back() {
            self.load_page(url, cx);
        }
    }

    fn forward(&mut self, cx: &mut Context<Self>) {
        if let Some(url) = self.history.forward() {
            self.load_page(url, cx);
        }
    }

    fn open_in_browser(&mut self, cx: &mut Context<Self>) {
        if let Some(url) = self.history.current() {
            cx.open_url(url);
        }
    }

    fn placeholder(&self, cx: &App) -> &'static str {
        let Some(session) = &self.session else {
            return "R isn't running in this window.";
        };
        match session.read(cx).state() {
            SessionState::Idle | SessionState::Busy => {
                if self.web_view.read(cx).is_available() {
                    "Search for a topic above, run ?topic in the console, or press F1 on a function."
                } else {
                    "Help pages open in your browser."
                }
            }
            SessionState::Starting | SessionState::Restarting => "Starting R…",
            SessionState::Restricted => "R starts once you trust this folder.",
            SessionState::Unavailable(_) | SessionState::Exited(_) => "R isn't running.",
        }
    }

    #[cfg(test)]
    fn pages(&self) -> (Vec<String>, usize) {
        (self.history.pages.clone(), self.history.index)
    }
}

impl Render for HelpPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.restyle(cx);
        let running = self.is_running(cx);
        let has_page = self.history.current().is_some();
        v_flex()
            .key_context("HelpPanel")
            .on_action(cx.listener(Self::search))
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        IconButton::new("help-back", IconName::ArrowLeft)
                            .icon_size(IconSize::Small)
                            .disabled(!self.history.can_go_back())
                            .tooltip(Tooltip::text("Back"))
                            .on_click(cx.listener(|this, _, _, cx| this.back(cx))),
                    )
                    .child(
                        IconButton::new("help-forward", IconName::ArrowRight)
                            .icon_size(IconSize::Small)
                            .disabled(!self.history.can_go_forward())
                            .tooltip(Tooltip::text("Forward"))
                            .on_click(cx.listener(|this, _, _, cx| this.forward(cx))),
                    )
                    .child(
                        IconButton::new("help-home", IconName::Book)
                            .icon_size(IconSize::Small)
                            .disabled(!running)
                            .tooltip(Tooltip::text("Help Index"))
                            .on_click(cx.listener(|this, _, window, cx| this.home(window, cx))),
                    )
                    .child(div().flex_1().min_w_0().px_1().child(self.topic.clone()))
                    .child(
                        IconButton::new("help-open-in-browser", IconName::ArrowUpRight)
                            .icon_size(IconSize::Small)
                            .disabled(!has_page)
                            .tooltip(Tooltip::text("Open in Browser"))
                            .on_click(cx.listener(|this, _, _, cx| this.open_in_browser(cx))),
                    ),
            )
            .children(self.notice.clone().map(|notice| {
                div().px_2().py_1().child(
                    Label::new(notice)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            }))
            .child(if has_page {
                div()
                    .flex_1()
                    .min_h_0()
                    .track_focus(&self.page_focus)
                    .child(web_view(&self.web_view, cx))
                    .into_any_element()
            } else {
                div()
                    .flex_1()
                    .p_4()
                    .child(
                        Label::new(self.placeholder(cx))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .into_any_element()
            })
    }
}

impl Focusable for HelpPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.topic.focus_handle(cx)
    }
}

impl EventEmitter<PanelEvent> for HelpPanel {}

impl Panel for HelpPanel {
    fn persistent_name() -> &'static str {
        "Help"
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
        px(420.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::CircleHelp)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Help")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn starts_open(&self, _: &Window, _: &App) -> bool {
        false
    }

    fn activation_priority(&self) -> u32 {
        12
    }

    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.active = active;
        self.update_shown(cx);
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
    use serde_json::{Value, json};
    use settings::SettingsStore;
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use web_view::{NativeEvent, fake_factory, take_calls};
    use workspace::MultiWorkspace;

    const MEAN: &str = "http://127.0.0.1:4321/library/base/html/mean.html";
    const LM: &str = "http://127.0.0.1:4321/library/stats/html/lm.html";
    const MEDIAN: &str = "http://127.0.0.1:4321/library/stats/html/median.html";

    struct Harness {
        workspace: Entity<Workspace>,
        panel: Entity<HelpPanel>,
        session: Entity<RSession>,
        web_view: Entity<WebView>,
        calls: Rc<RefCell<Vec<String>>>,
        requests: mpsc::Receiver<JupyterMessage>,
        help_comm: CommId,
    }

    fn init(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            super::init(cx);
        });
    }

    fn object(value: Value) -> serde_json::Map<String, Value> {
        match value {
            Value::Object(object) => object,
            other => panic!("expected an object, got {other}"),
        }
    }

    impl Harness {
        /// The workspace, with a console and a Help pane, for `project`,
        /// whose first folder is R's project root.
        async fn new<'a>(
            project: Entity<Project>,
            project_root: &Path,
            cx: &'a mut TestAppContext,
        ) -> (Self, &'a mut VisualTestContext) {
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
                    project_root.to_path_buf(),
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
            while requests.try_recv().is_ok() {}
            let help_comm = CommId(
                session.read_with(cx, |session, _| session.help_comm_id().unwrap().to_string()),
            );

            let calls: Rc<RefCell<Vec<String>>> = Rc::default();
            let web_view =
                cx.new(|cx| WebView::with_factory(Some(fake_factory(calls.clone())), cx));
            let panel = workspace.update_in(cx, |workspace, window, cx| {
                let console = cx.new(|cx| {
                    let mut console = ConsolePanel::new(workspace, window, cx);
                    console.set_session(session.clone(), window, cx);
                    console
                });
                workspace.add_panel(console, window, cx);
                let panel = cx.new(|cx| {
                    let mut panel = HelpPanel::new(workspace, web_view.clone(), window, cx);
                    panel.set_session(session.clone(), window, cx);
                    panel
                });
                workspace.add_panel(panel.clone(), window, cx);
                panel
            });
            let harness = Self {
                workspace,
                panel,
                session,
                web_view,
                calls,
                requests,
                help_comm,
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

        /// Ark sending a page, as after `?mean`.
        fn show_help(&self, url: &str, cx: &mut VisualTestContext) {
            self.receive(
                JupyterMessage::new(
                    CommMsg {
                        comm_id: self.help_comm.clone(),
                        data: object(json!({ "method": "show_help", "params": {
                            "content": url, "kind": "url", "focus": true
                        }})),
                    },
                    None,
                ),
                cx,
            );
        }

        /// WebKit reporting a page loaded, as after following a link.
        fn loaded(&self, url: &str, cx: &mut VisualTestContext) {
            let events = self
                .web_view
                .read_with(cx, |web_view, _| web_view.events_for_test().unwrap());
            events
                .unbounded_send(NativeEvent::Loaded(url.to_string()))
                .unwrap();
            cx.run_until_parked();
        }

        /// The pages loaded and visibility changes since the last call.
        fn loads(&self) -> Vec<String> {
            take_calls(&self.calls)
                .into_iter()
                .filter(|call| call.starts_with("load") || call.starts_with("visible"))
                .collect()
        }

        fn pages(&self, cx: &mut VisualTestContext) -> (Vec<String>, usize) {
            self.panel.read_with(cx, |panel, _| panel.pages())
        }

        fn notice(&self, cx: &mut VisualTestContext) -> Option<String> {
            self.panel.read_with(cx, |panel, _| panel.notice.clone())
        }

        fn is_open(&self, cx: &mut VisualTestContext) -> bool {
            self.workspace.read_with(cx, |workspace, cx| {
                workspace
                    .panel::<HelpPanel>(cx)
                    .is_some_and(|panel| panel.read(cx).active)
            })
        }

        /// Answers the pending `show_help_topic` request, returning its topic.
        fn answer_topic(&mut self, found: bool, cx: &mut VisualTestContext) -> String {
            let request = std::iter::from_fn(|| self.requests.try_recv().ok())
                .find(|request| {
                    matches!(&request.content, JupyterMessageContent::CommMsg(message)
                        if message.data["method"] == "show_help_topic")
                })
                .expect("a show_help_topic request");
            let JupyterMessageContent::CommMsg(message) = &request.content else {
                unreachable!();
            };
            assert_eq!(message.comm_id, self.help_comm);
            let topic = message.data["params"]["topic"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            self.receive(
                JupyterMessage::new(
                    CommMsg {
                        comm_id: self.help_comm.clone(),
                        data: object(json!({ "method": "show_help_topic", "result": found })),
                    },
                    Some(&request),
                ),
                cx,
            );
            topic
        }
    }

    async fn empty_project(cx: &mut TestAppContext) -> Entity<Project> {
        let fs = FakeFs::new(cx.executor());
        Project::test(fs, [] as [&Path; 0], cx).await
    }

    #[gpui::test]
    async fn test_pages_links_and_history(cx: &mut TestAppContext) {
        init(cx);
        let project = empty_project(cx).await;
        let (harness, cx) = Harness::new(project, Path::new("/projects/analysis"), cx).await;
        assert!(!harness.is_open(cx));

        // `?mean` shows the page and brings the pane to the front.
        harness.show_help(MEAN, cx);
        assert!(harness.is_open(cx));
        assert_eq!(
            harness.loads(),
            ["visible true".into(), format!("load {MEAN}")]
        );
        harness.loaded(MEAN, cx);
        assert_eq!(harness.pages(cx), (vec![MEAN.to_string()], 0));

        // Following a link adds a page; back and forward step through them.
        harness.loaded(LM, cx);
        assert_eq!(harness.pages(cx), (vec![MEAN.into(), LM.into()], 1));
        harness.panel.update(cx, |panel, cx| panel.back(cx));
        assert_eq!(harness.loads(), [format!("load {MEAN}")]);
        harness.loaded(MEAN, cx);
        assert_eq!(harness.pages(cx), (vec![MEAN.into(), LM.into()], 0));
        harness.panel.update(cx, |panel, cx| panel.forward(cx));
        assert_eq!(harness.loads(), [format!("load {LM}")]);
        harness.loaded(LM, cx);

        // A link followed after going back replaces the pages forward of it.
        harness.panel.update(cx, |panel, cx| panel.back(cx));
        harness.loaded(MEAN, cx);
        harness.loaded(MEDIAN, cx);
        assert_eq!(harness.pages(cx), (vec![MEAN.into(), MEDIAN.into()], 1));

        // Asking for a page shown before loads it again, as links may have
        // left it, and a redirect replaces it rather than adding a page.
        harness.loads();
        harness.show_help(MEAN, cx);
        assert_eq!(harness.loads(), [format!("load {MEAN}")]);
        let redirected = format!("{MEAN}#top");
        harness.loaded(&redirected, cx);
        assert_eq!(
            harness.pages(cx),
            (vec![MEAN.into(), MEDIAN.into(), redirected], 2)
        );

        // Pages are styled from the theme and fonts, and follow them.
        let style = |calls: Vec<String>| {
            calls
                .into_iter()
                .filter(|call| call.starts_with("style") || call.starts_with("create"))
                .collect::<Vec<_>>()
        };
        harness.loads();
        cx.update(|_, cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.theme.ui_font_size = Some(21.0.into());
                });
            });
        });
        cx.run_until_parked();
        let restyled = style(take_calls(&harness.calls));
        assert_eq!(restyled.len(), 1, "{restyled:?}");
        assert!(restyled[0].contains("--vscode-font-size: 21px;"));
        assert!(restyled[0].contains("--vscode-editor-background: rgba("));
        // Zooming the UI font doesn't change settings, but pages follow it too.
        cx.update(|_, cx| theme_settings::adjust_ui_font_size(cx, |size| size + px(2.)));
        cx.run_until_parked();
        let restyled = style(take_calls(&harness.calls));
        assert_eq!(restyled.len(), 1, "{restyled:?}");
        assert!(restyled[0].contains("--vscode-font-size: 23px;"));

        // A clicked page has the keyboard until GPUI's focus moves elsewhere.
        let events = harness
            .web_view
            .read_with(cx, |web_view, _| web_view.events_for_test().unwrap());
        events.unbounded_send(NativeEvent::Clicked).unwrap();
        cx.run_until_parked();
        assert!(cx.update(|window, cx| { harness.panel.read(cx).page_focus.is_focused(window) }));
        let topic = harness.panel.read_with(cx, |panel, _| panel.topic.clone());
        topic.update_in(cx, |topic, window, cx| {
            topic.focus_handle(cx).focus(window, cx)
        });
        cx.run_until_parked();
        assert!(take_calls(&harness.calls).contains(&"release keyboard".to_string()));

        // Under a modal, the page moves out of sight and a snapshot shows instead.
        harness.workspace.update_in(cx, |workspace, window, cx| {
            workspace.toggle_modal(window, cx, |_, cx| Dialog(cx.focus_handle()))
        });
        cx.run_until_parked();
        let calls = take_calls(&harness.calls);
        assert!(calls.contains(&"snapshot".to_string()), "{calls:?}");
        assert!(!calls.contains(&"visible false".to_string()), "{calls:?}");
    }

    #[gpui::test]
    async fn test_topic_search(cx: &mut TestAppContext) {
        init(cx);
        let project = empty_project(cx).await;
        let (mut harness, cx) = Harness::new(project, Path::new("/projects/analysis"), cx).await;
        let panel = harness.panel.clone();
        let search = |topic: &str, cx: &mut VisualTestContext| {
            let editor = panel.read_with(cx, |panel, _| panel.topic.clone());
            editor.update_in(cx, |editor, window, cx| editor.set_text(topic, window, cx));
            panel.update_in(cx, |panel, window, cx| {
                panel.search(&menu::Confirm, window, cx)
            });
            cx.run_until_parked();
        };

        search("  nosuchtopic ", cx);
        assert_eq!(harness.answer_topic(false, cx), "nosuchtopic");
        assert_eq!(
            harness.notice(cx).as_deref(),
            Some("No help found for `nosuchtopic`.")
        );

        search("lm", cx);
        assert_eq!(harness.answer_topic(true, cx), "lm");
        assert_eq!(harness.notice(cx), None);
        harness.show_help(LM, cx);
        assert_eq!(harness.pages(cx), (vec![LM.to_string()], 0));
    }

    struct Dialog(FocusHandle);

    impl Render for Dialog {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    impl Focusable for Dialog {
        fn focus_handle(&self, _: &App) -> FocusHandle {
            self.0.clone()
        }
    }

    impl EventEmitter<gpui::DismissEvent> for Dialog {}

    impl workspace::ModalView for Dialog {}

    enum HelpTopicRequest {}

    impl lsp::request::Request for HelpTopicRequest {
        type Params = Value;
        type Result = Option<Value>;
        const METHOD: &'static str = "positron/textDocument/helpTopic";
    }

    #[gpui::test]
    async fn test_f1_in_an_r_file_and_the_console(cx: &mut TestAppContext) {
        use futures::StreamExt as _;
        use std::sync::{Arc, Mutex};

        init(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            json!({ "analysis.R": "fit <- stats::lm(mpg ~ wt, mtcars)\n" }),
        )
        .await;
        let project = Project::test(fs, ["/project".as_ref()], cx).await;
        let languages = project.read_with(cx, |project, _| project.languages().clone());
        languages.add(languages::language("r", tree_sitter_r::LANGUAGE.into()));
        // What Ark was asked about, as (document, line, column).
        let asked: Arc<Mutex<Vec<(String, u64, u64)>>> = Arc::default();
        let mut fake_servers = languages.register_fake_lsp(
            runtime::R_LANGUAGE_NAME,
            language::FakeLspAdapter {
                name: "ark",
                initializer: Some(Box::new({
                    let asked = asked.clone();
                    move |server| {
                        let asked = asked.clone();
                        server.set_request_handler::<HelpTopicRequest, _, _>(move |params, _| {
                            let uri = params["textDocument"]["uri"].as_str().unwrap_or_default();
                            let document = if uri.ends_with("/project/analysis.R") {
                                "analysis.R"
                            } else if uri.contains("rezstudio-console") {
                                "console"
                            } else {
                                "other"
                            };
                            let line = params["position"]["line"].as_u64().unwrap_or_default();
                            let column =
                                params["position"]["character"].as_u64().unwrap_or_default();
                            asked
                                .lock()
                                .unwrap()
                                .push((document.to_string(), line, column));
                            let topic = match (document, column) {
                                ("analysis.R", _) => Some(json!({ "topic": "stats::lm" })),
                                ("console", 2..) => Some(json!({ "topic": "median" })),
                                _ => None,
                            };
                            async move { Ok(topic) }
                        });
                    }
                })),
                ..Default::default()
            },
        );
        let (harness, cx) = Harness::new(project.clone(), Path::new("/project"), cx).await;
        let mut harness = harness;
        let editor = harness
            .workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    PathBuf::from("/project/analysis.R"),
                    workspace::OpenOptions::default(),
                    window,
                    cx,
                )
            })
            .await
            .unwrap()
            .downcast::<Editor>()
            .unwrap();
        let _server = fake_servers.next().await.unwrap();
        cx.run_until_parked();

        // F1 on `lm` in the R file asks Ark about the file itself.
        editor.update_in(cx, |editor, window, cx| {
            editor.focus_handle(cx).focus(window, cx);
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_ranges([language::Point::new(0, 14)..language::Point::new(0, 14)])
            });
        });
        cx.dispatch_action(ShowHelpAtCursor);
        cx.run_until_parked();
        assert_eq!(
            *asked.lock().unwrap(),
            [("analysis.R".to_string(), 0, 14)],
            "{:?}",
            harness.notice(cx)
        );
        assert_eq!(harness.answer_topic(true, cx), "stats::lm");

        // In the console, Ark is asked about the console's own document.
        let console = harness
            .workspace
            .read_with(cx, |workspace, cx| workspace.panel::<ConsolePanel>(cx))
            .unwrap();
        let input = console.read_with(cx, |console, _| console.input_for_test());
        input.update_in(cx, |input, window, cx| {
            input.focus_handle(cx).focus(window, cx);
            input.set_text("median(x)", window, cx);
            input.change_selections(Default::default(), window, cx, |selections| {
                selections.select_ranges([language::Point::new(0, 3)..language::Point::new(0, 3)])
            });
        });
        asked.lock().unwrap().clear();
        cx.dispatch_action(ShowHelpAtCursor);
        cx.run_until_parked();
        assert_eq!(*asked.lock().unwrap(), [("console".to_string(), 0, 3)]);
        assert_eq!(harness.answer_topic(false, cx), "median");
        assert_eq!(
            harness.notice(cx).as_deref(),
            Some("No help found for `median`.")
        );
        assert!(harness.is_open(cx), "a miss shows why in the pane");

        // Where Ark finds nothing, nothing happens.
        input.update_in(cx, |input, window, cx| {
            input.change_selections(Default::default(), window, cx, |selections| {
                selections.select_ranges([language::Point::new(0, 0)..language::Point::new(0, 0)])
            });
        });
        harness.panel.update(cx, |panel, _| panel.notice = None);
        cx.dispatch_action(ShowHelpAtCursor);
        cx.run_until_parked();
        assert!(
            std::iter::from_fn(|| harness.requests.try_recv().ok())
                .next()
                .is_none()
        );
        assert_eq!(harness.notice(cx), None);
    }
}
