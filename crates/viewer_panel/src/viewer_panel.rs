//! The Viewer pane: htmlwidgets and local web apps such as Shiny, shown in a
//! web view, with a history of what was shown like the Plots pane's.

use anyhow::Result;
use gpui::{
    Action, App, AsyncWindowContext, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    Pixels, Subscription, WeakEntity, Window, actions, px,
};
use runtime::{RSession, SessionAdded, SessionEvent, SessionRegistry, SessionState};
use std::path::PathBuf;
use ui::{IconButton, IconName, Tooltip, prelude::*};
use util::ResultExt as _;
use web_view::{WebView, WebViewEvent, watch_cover};
use workspace::Workspace;
use workspace::dock::{DockPosition, PaneRoom, Panel, PanelEvent};

actions!(
    viewer_panel,
    [
        /// Opens the Viewer pane and moves focus to it.
        ToggleFocus,
    ]
);

const PANEL_KEY: &str = "ViewerPanel";
/// The height of the toolbar above a widget, roughly.
const TOOLBAR_HEIGHT: Pixels = px(32.);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<ViewerPanel>(window, cx);
        });
    })
    .detach();
}

/// Something R showed in the Viewer.
#[derive(Clone, Debug, PartialEq)]
enum Content {
    /// An HTML file, as htmlwidgets write to R's temporary directory with
    /// their dependencies beside them.
    File {
        path: PathBuf,
        title: String,
        /// The height R asked for in pixels: 0 for no preference, -1 for as
        /// tall as possible.
        height: i64,
    },
    /// A web app on this machine, such as Shiny's.
    App { url: String },
}

impl Content {
    fn url(&self) -> Option<String> {
        match self {
            Content::File { path, .. } => url::Url::from_file_path(path)
                .ok()
                .map(|url| url.to_string()),
            Content::App { url } => Some(url.clone()),
        }
    }
}

/// What was shown, oldest first, and which is showing.
#[derive(Default)]
struct History {
    contents: Vec<Content>,
    index: usize,
}

impl History {
    fn current(&self) -> Option<&Content> {
        self.contents.get(self.index)
    }

    /// Shows `content` after everything else, as the Plots pane does with a
    /// new plot.
    fn push(&mut self, content: Content) {
        self.contents.push(content);
        self.index = self.contents.len().saturating_sub(1);
    }

    fn can_go(&self, forward: bool) -> bool {
        if forward {
            self.index + 1 < self.contents.len()
        } else {
            self.index > 0
        }
    }

    fn go(&mut self, forward: bool) -> bool {
        if !self.can_go(forward) {
            return false;
        }
        if forward {
            self.index += 1;
        } else {
            self.index -= 1;
        }
        true
    }

    fn remove_current(&mut self) {
        if self.index < self.contents.len() {
            self.contents.remove(self.index);
        }
        self.index = self.index.min(self.contents.len().saturating_sub(1));
    }

    fn clear(&mut self) {
        self.contents.clear();
        self.index = 0;
    }

    fn position_label(&self) -> Option<String> {
        (self.contents.len() > 1).then(|| format!("{} / {}", self.index + 1, self.contents.len()))
    }
}

pub struct ViewerPanel {
    workspace: WeakEntity<Workspace>,
    workspace_id: EntityId,
    session: Option<Entity<RSession>>,
    web_view: Entity<WebView>,
    history: History,
    /// Whether the pane is the one shown in its dock.
    active: bool,
    focus_handle: FocusHandle,
    /// GPUI's focus while the page has the keyboard, so the page can give
    /// it back when focus moves on.
    page_focus: FocusHandle,
    position: DockPosition,
    _session_subscription: Option<Subscription>,
    _cover_subscriptions: Vec<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl ViewerPanel {
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
        let page_focus = cx.focus_handle();
        let mut subscriptions = vec![
            cx.subscribe_in(
                &web_view,
                window,
                |this, _, event, window, cx| match event {
                    WebViewEvent::Clicked => window.focus(&this.page_focus, cx),
                    WebViewEvent::TitleChanged(_) | WebViewEvent::Navigated(_) => cx.notify(),
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
            active: false,
            focus_handle: cx.focus_handle(),
            page_focus,
            position: DockPosition::Right,
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
        self.forget_contents(cx);
    }

    fn is_running(&self, cx: &App) -> bool {
        self.session.as_ref().is_some_and(|session| {
            matches!(
                session.read(cx).state(),
                SessionState::Idle | SessionState::Busy
            )
        })
    }

    fn is_busy(&self, cx: &App) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| *session.read(cx).state() == SessionState::Busy)
    }

    fn handle_session_event(
        &mut self,
        event: &SessionEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            // Every htmlwidget shows here, whichever pane R suggests.
            SessionEvent::ShowHtmlFile {
                path,
                title,
                height,
                ..
            } => self.show(
                Content::File {
                    path: path.clone(),
                    title: title.clone(),
                    height: *height,
                },
                window,
                cx,
            ),
            SessionEvent::ShowUrl { url } => {
                self.show(Content::App { url: url.clone() }, window, cx)
            }
            // R's temporary directory and its apps go with it.
            SessionEvent::StateChanged if !self.is_running(cx) => self.forget_contents(cx),
            SessionEvent::StateChanged => cx.notify(),
            _ => {}
        }
    }

    fn forget_contents(&mut self, cx: &mut Context<Self>) {
        self.history.clear();
        self.load_current(cx);
    }

    /// Shows `content` and brings the pane to the front.
    fn show(&mut self, content: Content, window: &mut Window, cx: &mut Context<Self>) {
        if !self.web_view.read(cx).is_available() {
            match &content {
                Content::File { path, .. } => cx.open_with_system(path),
                Content::App { url } => cx.open_url(url),
            }
            return;
        }
        // The same app shown again, as when it's run again on its port, reloads.
        if self.history.current() == Some(&content) {
            self.web_view.read(cx).reload();
        } else {
            self.history.push(content.clone());
            self.load_current(cx);
        }
        let room = match content {
            Content::File { height: -1, .. } => Some(PaneRoom::Most),
            Content::File { height, .. } if height > 0 => {
                Some(PaneRoom::AtLeast(px(height as f32) + TOOLBAR_HEIGHT))
            }
            _ => None,
        };
        self.reveal(room, window, cx);
    }

    fn load_current(&mut self, cx: &mut Context<Self>) {
        let current = self.history.current().cloned();
        self.web_view.update(cx, |web_view, cx| match &current {
            Some(Content::File { path, .. }) => web_view.load_file(path, cx),
            Some(Content::App { url }) => web_view.load_url(url.clone(), cx),
            None => {}
        });
        self.update_shown(cx);
        cx.notify();
    }

    /// Brings the pane to the front, with `room` if a widget asked for it.
    fn reveal(&mut self, room: Option<PaneRoom>, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        let position = self.position;
        let panel_id = cx.entity_id();
        // Opening a panel updates the docks, which can't happen mid-update.
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.open_panel::<ViewerPanel>(window, cx);
                    if let Some(room) = room {
                        workspace
                            .dock_at_position(position)
                            .update(cx, |dock, cx| dock.make_room_for(panel_id, room, cx));
                    }
                })
                .log_err();
        });
    }

    fn update_shown(&mut self, cx: &mut Context<Self>) {
        let shown = self.active && self.history.current().is_some();
        self.web_view
            .update(cx, |web_view, cx| web_view.set_shown(shown, cx));
    }

    fn go(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.history.go(forward) {
            self.load_current(cx);
        }
    }

    fn remove_current(&mut self, cx: &mut Context<Self>) {
        self.history.remove_current();
        self.load_current(cx);
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.history.clear();
        self.load_current(cx);
    }

    /// Stops an app by interrupting R, which is running it, as Shiny's
    /// console does.
    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = self.session.clone() {
            session
                .update(cx, |session, _| session.interrupt())
                .log_err();
        }
    }

    fn open_in_browser(&mut self, cx: &mut Context<Self>) {
        if let Some(url) = self.history.current().and_then(Content::url) {
            cx.open_url(&url);
        }
    }

    fn title(&self, cx: &App) -> Option<String> {
        let current = self.history.current()?;
        let page_title = self
            .web_view
            .read(cx)
            .title()
            .filter(|title| !title.is_empty())
            .map(str::to_string);
        Some(match current {
            Content::File { path, title, .. } => {
                if !title.is_empty() {
                    title.clone()
                } else if let Some(page_title) = page_title {
                    page_title
                } else {
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default()
                }
            }
            Content::App { url } => page_title.unwrap_or_else(|| url.clone()),
        })
    }

    fn placeholder(&self, cx: &App) -> &'static str {
        let Some(session) = &self.session else {
            return "R isn't running in this window.";
        };
        match session.read(cx).state() {
            SessionState::Idle | SessionState::Busy => {
                if self.web_view.read(cx).is_available() {
                    "htmlwidgets and Shiny apps show here."
                } else {
                    "htmlwidgets and Shiny apps open in your browser."
                }
            }
            SessionState::Starting | SessionState::Restarting => "Starting R…",
            SessionState::Restricted => "R starts once you trust this folder.",
            SessionState::Unavailable(_) | SessionState::Exited(_) => "R isn't running.",
        }
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let has_content = self.history.current().is_some();
        let running_app =
            matches!(self.history.current(), Some(Content::App { .. })) && self.is_busy(cx);
        h_flex()
            .px_2()
            .py_1()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                IconButton::new("previous-content", IconName::ChevronLeft)
                    .icon_size(IconSize::Small)
                    .disabled(!self.history.can_go(false))
                    .tooltip(Tooltip::text("Previous"))
                    .on_click(cx.listener(|this, _, _, cx| this.go(false, cx))),
            )
            .child(
                IconButton::new("next-content", IconName::ChevronRight)
                    .icon_size(IconSize::Small)
                    .disabled(!self.history.can_go(true))
                    .tooltip(Tooltip::text("Next"))
                    .on_click(cx.listener(|this, _, _, cx| this.go(true, cx))),
            )
            .children(
                self.history
                    .position_label()
                    .map(|label| Label::new(label).size(LabelSize::Small).color(Color::Muted)),
            )
            .child(
                div().flex_1().min_w_0().px_1().children(
                    self.title(cx)
                        .map(|title| Label::new(title).size(LabelSize::Small).truncate()),
                ),
            )
            .when(running_app, |this| {
                this.child(
                    IconButton::new("stop-app", IconName::Stop)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Error)
                        .tooltip(Tooltip::text("Stop App"))
                        .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                )
            })
            .child(
                IconButton::new("reload-content", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .disabled(!has_content)
                    .tooltip(Tooltip::text("Reload"))
                    .on_click(cx.listener(|this, _, _, cx| this.web_view.read(cx).reload())),
            )
            .child(
                IconButton::new("open-content-in-browser", IconName::ArrowUpRight)
                    .icon_size(IconSize::Small)
                    .disabled(!has_content)
                    .tooltip(Tooltip::text("Open in Browser"))
                    .on_click(cx.listener(|this, _, _, cx| this.open_in_browser(cx))),
            )
            .child(
                IconButton::new("remove-content", IconName::Close)
                    .icon_size(IconSize::Small)
                    .disabled(!has_content)
                    .tooltip(Tooltip::text("Remove"))
                    .on_click(cx.listener(|this, _, _, cx| this.remove_current(cx))),
            )
            .child(
                IconButton::new("clear-contents", IconName::Trash)
                    .icon_size(IconSize::Small)
                    .disabled(!has_content)
                    .tooltip(Tooltip::text("Clear All"))
                    .on_click(cx.listener(|this, _, _, cx| this.clear(cx))),
            )
    }

    #[cfg(test)]
    fn contents(&self) -> (Vec<Content>, usize) {
        (self.history.contents.clone(), self.history.index)
    }
}

impl Render for ViewerPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_content = self.history.current().is_some();
        v_flex()
            .key_context("ViewerPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_toolbar(cx))
            .child(if has_content {
                div()
                    .flex_1()
                    .min_h_0()
                    .track_focus(&self.page_focus)
                    .child(self.web_view.clone())
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

impl Focusable for ViewerPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ViewerPanel {}

impl Panel for ViewerPanel {
    fn persistent_name() -> &'static str {
        "Viewer"
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
        Some(IconName::Screen)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Viewer")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn starts_open(&self, _: &Window, _: &App) -> bool {
        false
    }

    fn activation_priority(&self) -> u32 {
        13
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
    use std::path::Path;
    use std::rc::Rc;
    use web_view::{fake_factory, take_calls};
    use workspace::MultiWorkspace;
    use workspace::dock::{DockLayout, DockPaneLayout, test::TestPanel};

    const WIDGET: &str = "/tmp/RtmpX/viewhtml1/index.html";
    const APP: &str = "http://127.0.0.1:7654/";

    struct Harness {
        workspace: Entity<Workspace>,
        panel: Entity<ViewerPanel>,
        session: Entity<RSession>,
        calls: Rc<RefCell<Vec<String>>>,
        requests: mpsc::Receiver<JupyterMessage>,
        ui_comm: CommId,
    }

    fn object(value: Value) -> serde_json::Map<String, Value> {
        match value {
            Value::Object(object) => object,
            other => panic!("expected an object, got {other}"),
        }
    }

    impl Harness {
        async fn new(cx: &mut TestAppContext) -> (Self, &mut VisualTestContext) {
            cx.update(|cx| {
                let settings = SettingsStore::test(cx);
                cx.set_global(settings);
                theme_settings::init(theme::LoadThemes::JustBase, cx);
                super::init(cx);
            });
            let fs = FakeFs::new(cx.executor());
            let project = Project::test(fs, [] as [&Path; 0], cx).await;
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

            let calls: Rc<RefCell<Vec<String>>> = Rc::default();
            let web_view =
                cx.new(|cx| WebView::with_factory(Some(fake_factory(calls.clone())), cx));
            let panel = workspace.update_in(cx, |workspace, window, cx| {
                let panel = cx.new(|cx| {
                    let mut panel = ViewerPanel::new(workspace, web_view, window, cx);
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
                calls,
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

        fn ui_event(&self, data: Value, cx: &mut VisualTestContext) {
            self.receive(
                JupyterMessage::new(
                    CommMsg {
                        comm_id: self.ui_comm.clone(),
                        data: object(data),
                    },
                    None,
                ),
                cx,
            );
        }

        fn show_widget(&self, path: &str, cx: &mut VisualTestContext) {
            self.show_widget_with_height(path, 400, cx);
        }

        fn show_widget_with_height(&self, path: &str, height: i64, cx: &mut VisualTestContext) {
            self.ui_event(
                json!({ "method": "show_html_file", "params": {
                    "path": path, "title": "plotly", "destination": "plot", "height": height
                }}),
                cx,
            );
        }

        fn show_app(&self, url: &str, cx: &mut VisualTestContext) {
            self.ui_event(
                json!({ "method": "show_url", "params": { "url": url } }),
                cx,
            );
        }

        /// The pages loaded, reloads and visibility changes since the last call.
        fn loads(&self) -> Vec<String> {
            take_calls(&self.calls)
                .into_iter()
                .filter(|call| {
                    call.starts_with("load") || call.starts_with("visible") || call == "reload"
                })
                .collect()
        }

        fn shown(&self, cx: &mut VisualTestContext) -> Vec<String> {
            let (contents, index) = self.panel.read_with(cx, |panel, _| panel.contents());
            contents
                .iter()
                .enumerate()
                .map(|(position, content)| {
                    let name = match content {
                        Content::File { path, .. } => path.display().to_string(),
                        Content::App { url } => url.clone(),
                    };
                    if position == index {
                        format!("[{name}]")
                    } else {
                        name
                    }
                })
                .collect()
        }

        fn is_open(&self, cx: &mut VisualTestContext) -> bool {
            self.workspace.read_with(cx, |workspace, cx| {
                workspace
                    .panel::<ViewerPanel>(cx)
                    .is_some_and(|panel| panel.read(cx).active)
            })
        }
    }

    #[gpui::test]
    async fn test_widgets_and_apps(cx: &mut TestAppContext) {
        let (harness, cx) = Harness::new(cx).await;
        assert!(!harness.is_open(cx));

        // An htmlwidget shows from its folder and brings the pane to the front.
        harness.show_widget(WIDGET, cx);
        assert!(harness.is_open(cx));
        assert_eq!(
            harness.loads(),
            [
                "visible true".to_string(),
                format!("load rezfile://localhost{WIDGET}")
            ]
        );
        assert_eq!(
            harness.panel.read_with(cx, |panel, cx| panel.title(cx)),
            Some("plotly".to_string())
        );

        // A local app comes after it; back and forward step between them.
        harness.show_app(APP, cx);
        assert_eq!(harness.loads(), [format!("load {APP}")]);
        assert_eq!(harness.shown(cx), [WIDGET.to_string(), format!("[{APP}]")]);
        harness.panel.update(cx, |panel, cx| panel.go(false, cx));
        assert_eq!(
            harness.loads(),
            [format!("load rezfile://localhost{WIDGET}")]
        );
        harness.panel.update(cx, |panel, cx| panel.go(true, cx));
        assert_eq!(harness.loads(), [format!("load {APP}")]);

        // The same app again reloads rather than adding to the history.
        harness.show_app(APP, cx);
        assert_eq!(harness.loads(), ["reload"]);
        assert_eq!(harness.shown(cx).len(), 2);

        // Removing shows what's left; clearing hides the view.
        harness
            .panel
            .update(cx, |panel, cx| panel.remove_current(cx));
        assert_eq!(harness.shown(cx), [format!("[{WIDGET}]")]);
        assert_eq!(
            harness.loads(),
            [format!("load rezfile://localhost{WIDGET}")]
        );
        harness.panel.update(cx, |panel, cx| panel.clear(cx));
        assert!(harness.shown(cx).is_empty());
        assert_eq!(harness.loads(), ["visible false"]);
    }

    #[gpui::test]
    async fn test_widgets_get_the_room_they_ask_for(cx: &mut TestAppContext) {
        let (harness, cx) = Harness::new(cx).await;
        let dock = harness.workspace.update_in(cx, |workspace, window, cx| {
            let other = cx.new(|cx| TestPanel::new(DockPosition::Right, 100, cx));
            workspace.add_panel(other, window, cx);
            workspace.toggle_dock(DockPosition::Right, window, cx);
            let dock = workspace.right_dock().clone();
            dock.update(cx, |dock, cx| {
                dock.set_layout(
                    DockLayout {
                        panes: vec![
                            DockPaneLayout {
                                panels: vec!["TestPanel".to_string()],
                                active: None,
                            },
                            DockPaneLayout {
                                panels: vec!["Viewer".to_string()],
                                active: None,
                            },
                        ],
                        divider: 0.5,
                    },
                    window,
                    cx,
                )
            });
            dock
        });
        cx.run_until_parked();
        let divider =
            |cx: &mut VisualTestContext| dock.read_with(cx, |dock, _| dock.layout().divider);
        let reset = |cx: &mut VisualTestContext| {
            dock.update_in(cx, |dock, window, cx| {
                let mut layout = dock.layout().clone();
                layout.divider = 0.5;
                dock.set_layout(layout, window, cx);
            });
            cx.run_until_parked();
        };

        // No preference leaves the panes as they are.
        harness.show_widget_with_height(WIDGET, 0, cx);
        assert_eq!(divider(cx), 0.5);

        // A widget's height, plus the toolbar above it, fits in the Viewer's pane.
        harness.show_widget_with_height("/tmp/RtmpX/viewhtml2/index.html", 500, cx);
        assert!(divider(cx) < 0.5, "{}", divider(cx));
        let viewer_pane = cx
            .debug_bounds("dock-pane-1")
            .expect("the Viewer's pane is drawn");
        assert!(
            viewer_pane.size.height >= px(500.) + TOOLBAR_HEIGHT - px(1.),
            "{:?}",
            viewer_pane.size.height
        );

        // -1 asks for as much as the other pane allows.
        reset(cx);
        harness.show_widget_with_height("/tmp/RtmpX/viewhtml3/index.html", -1, cx);
        assert!((divider(cx) - 0.1).abs() < 0.001, "{}", divider(cx));
    }

    #[gpui::test]
    async fn test_stop_interrupts_the_app(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        // `shiny::runApp()` keeps R busy while the app runs.
        harness
            .session
            .update(cx, |session, cx| {
                session.execute("shiny::runApp()".into(), cx)
            })
            .unwrap();
        harness.show_app(APP, cx);
        while harness.requests.try_recv().is_ok() {}
        assert!(harness.panel.read_with(cx, |panel, cx| panel.is_busy(cx)));

        harness.panel.update(cx, |panel, cx| panel.stop(cx));
        let interrupt = std::iter::from_fn(|| harness.requests.try_recv().ok())
            .find(|request| request.content.message_type() == "interrupt_request");
        assert!(interrupt.is_some());
    }
}
