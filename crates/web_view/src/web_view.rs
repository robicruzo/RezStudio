//! A web page inside a GPUI window, for R help, htmlwidgets and Shiny apps.
//!
//! On macOS this is a WKWebView (through `wry`) added as a child of the
//! window's view and moved to the element's bounds whenever it paints. A
//! native view draws over everything GPUI paints in its rectangle, so its
//! owner says when it may show; [`watch_cover`] reports when a modal, a
//! zoomed item or another workspace would put GPUI content on top of it.
//! Other platforms have no web view; pages open in the browser instead.

mod files;
#[cfg(target_os = "macos")]
mod native;

use anyhow::Result;
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{
    App, Bounds, Context, Entity, EntityId, EventEmitter, Pixels, Rgba, Subscription, Task, Window,
    canvas,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{MultiWorkspace, MultiWorkspaceEvent, Workspace};

pub use files::{file_url, is_allowed_url};

/// What the page reports, from WebKit's callbacks.
#[derive(Debug, PartialEq)]
pub enum NativeEvent {
    TitleChanged(String),
    /// A page finished loading, at this URL.
    Loaded(String),
    /// A link to somewhere the view doesn't show, for the browser.
    OpenExternally(String),
    /// The page was clicked, which gives it the keyboard.
    Clicked,
}

/// How a view's pages look, for pages whose owner styles them, as the Help
/// pane does R's help to match the app's theme.
#[derive(Clone, Debug, PartialEq)]
pub struct PageStyle {
    /// Added to every page as it starts, before it draws.
    pub css: String,
    /// Shown where there's no page, as between pages, instead of white.
    pub background: Rgba,
}

/// The script that puts `css` in a page, in one `<style>` element it
/// replaces when run again. At document start there may be no `<head>`
/// yet, so it goes in `<html>`, before the page's own stylesheets.
pub fn page_style_script(css: &str) -> Result<String> {
    let css = serde_json::to_string(css)?;
    Ok(format!(
        r#"(() => {{
  const apply = () => {{
    let style = document.getElementById("rezstudio-page-style");
    if (!style) {{
      style = document.createElement("style");
      style.id = "rezstudio-page-style";
      document.documentElement.prepend(style);
    }}
    style.textContent = {css};
  }};
  if (document.documentElement) {{
    apply();
  }} else {{
    document.addEventListener("readystatechange", apply, {{ once: true }});
  }}
}})();"#
    ))
}

/// The native view, behind a trait so tests don't need WebKit.
pub trait NativeView {
    fn set_bounds(&self, bounds: Bounds<Pixels>) -> Result<()>;
    fn set_visible(&self, visible: bool) -> Result<()>;
    fn load_url(&self, url: &str) -> Result<()>;
    fn evaluate_script(&self, script: &str) -> Result<()>;
    fn reload(&self) -> Result<()>;
    /// Restyles the page shown and every page after it.
    fn set_page_style(&self, style: &PageStyle) -> Result<()>;
    /// Gives keyboard input back to the window's GPUI view if the page has
    /// it. Hiding the view does this too.
    fn release_keyboard(&self) -> Result<()>;
}

/// Makes the native view for a window, which needs the window's handle,
/// styling its pages with the style given, if any.
pub type NativeFactory = Box<
    dyn Fn(
        &Window,
        Bounds<Pixels>,
        mpsc::UnboundedSender<NativeEvent>,
        Arc<Mutex<Option<PathBuf>>>,
        Option<PageStyle>,
    ) -> Result<Box<dyn NativeView>>,
>;

#[derive(Clone, Debug, PartialEq)]
pub enum WebViewEvent {
    TitleChanged(String),
    Navigated(String),
    /// The page was clicked and has the keyboard. Its owner should take
    /// GPUI's focus, and call [`WebView::release_keyboard`] when it loses it.
    Clicked,
}

pub struct WebView {
    factory: Option<NativeFactory>,
    native: Option<Box<dyn NativeView>>,
    /// Why there's no native view, when it couldn't be made.
    unavailable: Option<String>,
    /// The page to show, and the one the native view was last told to load.
    url: Option<String>,
    loaded: Option<String>,
    /// Where pages say they are and what they're called, from WebKit.
    current_url: Option<String>,
    title: Option<String>,
    /// The folder `rezfile://` URLs may read from: the shown file's.
    file_root: Arc<Mutex<Option<PathBuf>>>,
    /// Whether the owner lets it show: its pane is visible and nothing covers it.
    shown: bool,
    page_style: Option<PageStyle>,
    bounds: Option<Bounds<Pixels>>,
    native_visible: Option<bool>,
    events: Option<mpsc::UnboundedSender<NativeEvent>>,
    _events_task: Option<Task<()>>,
}

impl EventEmitter<WebViewEvent> for WebView {}

impl WebView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        #[cfg(target_os = "macos")]
        let factory: Option<NativeFactory> = Some(Box::new(native::create));
        #[cfg(not(target_os = "macos"))]
        let factory: Option<NativeFactory> = None;
        Self::with_factory(factory, cx)
    }

    pub fn with_factory(factory: Option<NativeFactory>, cx: &mut Context<Self>) -> Self {
        let (events_tx, mut events_rx) = mpsc::unbounded();
        let events_task = cx.spawn(async move |this, cx| {
            while let Some(event) = events_rx.next().await {
                if this
                    .update(cx, |this, cx| this.handle_native_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            unavailable: factory
                .is_none()
                .then(|| "This build can't show web pages.".to_string()),
            factory,
            native: None,
            url: None,
            loaded: None,
            current_url: None,
            title: None,
            file_root: Arc::default(),
            shown: false,
            page_style: None,
            bounds: None,
            native_visible: None,
            events: Some(events_tx),
            _events_task: Some(events_task),
        }
    }

    /// The page it's showing, or was last asked to.
    pub fn url(&self) -> Option<&str> {
        self.current_url.as_deref().or(self.url.as_deref())
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn is_available(&self) -> bool {
        self.unavailable.is_none()
    }

    /// Loads `url`, even if it was the last one loaded: links may have led
    /// elsewhere since.
    pub fn load_url(&mut self, url: impl Into<String>, cx: &mut Context<Self>) {
        self.url = Some(url.into());
        self.loaded = None;
        self.title = None;
        self.current_url = None;
        self.apply(cx);
    }

    /// Shows a local HTML file, letting it read files beside it, as an
    /// htmlwidget's `*_files/` dependencies are.
    pub fn load_file(&mut self, path: &Path, cx: &mut Context<Self>) {
        if let Ok(mut root) = self.file_root.lock() {
            *root = path.parent().map(Path::to_path_buf);
        }
        self.load_url(file_url(path), cx);
    }

    pub fn back(&self) {
        self.run_script("history.back()");
    }

    pub fn forward(&self) {
        self.run_script("history.forward()");
    }

    pub fn reload(&self) {
        if let Some(native) = &self.native {
            native.reload().log_err();
        }
    }

    fn run_script(&self, script: &str) {
        if let Some(native) = &self.native {
            native.evaluate_script(script).log_err();
        }
    }

    /// Styles every page from now on, and the one shown now.
    pub fn set_page_style(&mut self, style: PageStyle, _cx: &mut Context<Self>) {
        if self.page_style.as_ref() == Some(&style) {
            return;
        }
        if let Some(native) = &self.native {
            native.set_page_style(&style).log_err();
        }
        self.page_style = Some(style);
    }

    /// Gives keyboard input back to GPUI, as when GPUI's focus moves off the
    /// page's pane.
    pub fn release_keyboard(&self) {
        if let Some(native) = &self.native {
            native.release_keyboard().log_err();
        }
    }

    /// Whether it may show; the owner calls this when its pane is shown or
    /// hidden, and when something would cover it.
    pub fn set_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        self.shown = shown;
        self.apply(cx);
    }

    /// Called as the element paints: moves the view to `bounds`, making it
    /// first if it hasn't been.
    fn place(&mut self, bounds: Bounds<Pixels>, window: &Window, cx: &mut Context<Self>) {
        if self.native.is_none()
            && self.unavailable.is_none()
            && let (Some(factory), Some(events)) = (&self.factory, &self.events)
        {
            match factory(
                window,
                bounds,
                events.clone(),
                self.file_root.clone(),
                self.page_style.clone(),
            ) {
                Ok(native) => self.native = Some(native),
                Err(error) => {
                    log::error!("couldn't make a web view: {error:#}");
                    self.unavailable = Some(format!("Couldn't show the page: {error:#}"));
                    cx.notify();
                }
            }
            self.bounds = None;
            self.native_visible = None;
        }
        if self.bounds != Some(bounds) {
            self.bounds = Some(bounds);
            if let Some(native) = &self.native {
                native.set_bounds(bounds).log_err();
            }
        }
        self.apply(cx);
    }

    /// Brings the native view in line: visible only when shown and placed,
    /// and loading the page asked for.
    fn apply(&mut self, _cx: &mut Context<Self>) {
        let Some(native) = &self.native else {
            return;
        };
        let visible = self.shown && self.bounds.is_some();
        if self.native_visible != Some(visible) {
            native.set_visible(visible).log_err();
            self.native_visible = Some(visible);
        }
        if self.url != self.loaded
            && let Some(url) = &self.url
        {
            native.load_url(url).log_err();
            self.loaded = self.url.clone();
        }
    }

    fn handle_native_event(&mut self, event: NativeEvent, cx: &mut Context<Self>) {
        match event {
            NativeEvent::TitleChanged(title) => {
                self.title = Some(title.clone());
                cx.emit(WebViewEvent::TitleChanged(title));
            }
            NativeEvent::Loaded(url) => {
                self.current_url = Some(url.clone());
                cx.emit(WebViewEvent::Navigated(url));
            }
            NativeEvent::OpenExternally(url) => cx.open_url(&url),
            NativeEvent::Clicked => cx.emit(WebViewEvent::Clicked),
        }
        cx.notify();
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn events_for_test(&self) -> Option<mpsc::UnboundedSender<NativeEvent>> {
        self.events.clone()
    }
}

impl Drop for WebView {
    fn drop(&mut self) {
        // The native view is a child of the window's view; hide it at once so
        // it can't linger over the window until WebKit lets it go.
        if let Some(native) = &self.native {
            native.set_visible(false).log_err();
        }
    }
}

/// The element a web view fills, placing the native view at its bounds.
pub fn web_view(view: &Entity<WebView>, cx: &App) -> AnyElement {
    if let Some(reason) = view.read(cx).unavailable.clone() {
        return div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p_4()
            .child(Label::new(reason).color(Color::Muted))
            .into_any_element();
    }
    let view = view.clone();
    div()
        .relative()
        .size_full()
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, cx| {
                    view.update(cx, |view, cx| view.place(bounds, window, cx));
                },
            )
            .absolute()
            .size_full(),
        )
        .into_any_element()
}

/// Calls `on_change` with whether GPUI content would be drawn over a native
/// view in `own`'s pane: a modal, another item zoomed over the workspace, or
/// another workspace in the window. Keep the subscriptions alive.
pub fn watch_cover<T: 'static>(
    workspace: &Entity<Workspace>,
    own: EntityId,
    window: &mut Window,
    cx: &mut Context<T>,
    on_change: impl Fn(&mut T, bool, &mut Context<T>) + 'static,
) -> Vec<Subscription> {
    let on_change = std::rc::Rc::new(on_change);
    let workspace_handle = workspace.downgrade();
    let multi_workspace = window.root::<MultiWorkspace>().flatten();
    let covered = {
        let multi_workspace = multi_workspace.clone();
        move |cx: &App| -> bool {
            let Some(workspace) = workspace_handle.upgrade() else {
                return true;
            };
            let workspace_read = workspace.read(cx);
            let modal = workspace_read.modal_layer().read(cx).has_active_modal();
            let zoomed = workspace_read
                .zoomed_item()
                .and_then(|zoomed| zoomed.upgrade())
                .is_some_and(|zoomed| zoomed.entity_id() != own);
            let other_workspace = multi_workspace.as_ref().is_some_and(|multi| {
                multi.read(cx).workspace().entity_id() != workspace.entity_id()
            });
            modal || zoomed || other_workspace
        }
    };
    let covered = std::rc::Rc::new(covered);
    let mut subscriptions = vec![
        cx.observe(&workspace.read(cx).modal_layer().clone(), {
            let covered = covered.clone();
            let on_change = on_change.clone();
            move |this, _, cx| on_change(this, covered(cx), cx)
        }),
        cx.subscribe(workspace, {
            let covered = covered.clone();
            let on_change = on_change.clone();
            move |this, _, event: &workspace::Event, cx| {
                if matches!(
                    event,
                    workspace::Event::ZoomChanged | workspace::Event::ModalOpened
                ) {
                    on_change(this, covered(cx), cx);
                }
            }
        }),
    ];
    if let Some(multi_workspace) = multi_workspace {
        subscriptions.push(cx.subscribe(&multi_workspace, {
            move |this, _, event: &MultiWorkspaceEvent, cx| {
                if matches!(event, MultiWorkspaceEvent::ActiveWorkspaceChanged { .. }) {
                    on_change(this, covered(cx), cx);
                }
            }
        }));
    }
    subscriptions
}

/// A native view that records what it's told, one line per call, for tests.
#[cfg(any(test, feature = "test-support"))]
pub struct FakeNative(pub std::rc::Rc<std::cell::RefCell<Vec<String>>>);

#[cfg(any(test, feature = "test-support"))]
impl NativeView for FakeNative {
    fn set_bounds(&self, bounds: Bounds<Pixels>) -> Result<()> {
        self.0.borrow_mut().push(format!(
            "bounds {}x{}",
            f32::from(bounds.size.width),
            f32::from(bounds.size.height)
        ));
        Ok(())
    }
    fn set_visible(&self, visible: bool) -> Result<()> {
        self.0.borrow_mut().push(format!("visible {visible}"));
        Ok(())
    }
    fn load_url(&self, url: &str) -> Result<()> {
        self.0.borrow_mut().push(format!("load {url}"));
        Ok(())
    }
    fn evaluate_script(&self, script: &str) -> Result<()> {
        self.0.borrow_mut().push(format!("script {script}"));
        Ok(())
    }
    fn reload(&self) -> Result<()> {
        self.0.borrow_mut().push("reload".into());
        Ok(())
    }
    fn set_page_style(&self, style: &PageStyle) -> Result<()> {
        self.0.borrow_mut().push(format!("style {}", style.css));
        Ok(())
    }
    fn release_keyboard(&self) -> Result<()> {
        self.0.borrow_mut().push("release keyboard".into());
        Ok(())
    }
}

/// Makes [`FakeNative`]s that record into `calls`, after a "create" line.
#[cfg(any(test, feature = "test-support"))]
pub fn fake_factory(calls: std::rc::Rc<std::cell::RefCell<Vec<String>>>) -> NativeFactory {
    Box::new(move |_, _, _, _, style| {
        calls.borrow_mut().push(match style {
            Some(style) => format!("create with style {}", style.css),
            None => "create".into(),
        });
        Ok(Box::new(FakeNative(calls.clone())))
    })
}

/// The calls recorded since the last take.
#[cfg(any(test, feature = "test-support"))]
pub fn take_calls(calls: &std::rc::Rc<std::cell::RefCell<Vec<String>>>) -> Vec<String> {
    std::mem::take(&mut *calls.borrow_mut())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        DismissEvent, FocusHandle, Focusable, Render, TestAppContext, VisualTestContext, px, size,
    };
    use std::cell::RefCell;
    use std::rc::Rc;

    struct Host {
        view: Entity<WebView>,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(web_view(&self.view, cx))
        }
    }

    fn init_theme(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
    }

    #[gpui::test]
    fn test_page_style(cx: &mut TestAppContext) {
        init_theme(cx);
        let calls: Rc<RefCell<Vec<String>>> = Rc::default();
        let factory = fake_factory(calls.clone());
        let style = |css: &str| PageStyle {
            css: css.to_string(),
            background: gpui::rgba(0x282c33ff),
        };
        let (host, cx) = cx.add_window_view(|_, cx| {
            let view = cx.new(|cx| {
                let mut view = WebView::with_factory(Some(factory), cx);
                view.set_page_style(style("body { color: red }"), cx);
                view
            });
            Host { view }
        });
        cx.run_until_parked();
        // A style set before the view exists is part of making it.
        assert_eq!(
            take_calls(&calls).first().map(String::as_str),
            Some("create with style body { color: red }")
        );

        let view = host.read_with(cx, |host, _| host.view.clone());
        view.update(cx, |view, cx| {
            view.set_page_style(style("body { color: red }"), cx);
            view.set_page_style(style("body { color: blue }"), cx);
        });
        assert_eq!(take_calls(&calls), ["style body { color: blue }"]);

        // The CSS reaches the page as a string, quotes and all.
        let script = page_style_script("a::before { content: \"</style>\" }").unwrap();
        assert!(script.contains(r#"style.textContent = "a::before { content: \"</style>\" }";"#));
    }

    #[gpui::test]
    fn test_placed_shown_and_loaded(cx: &mut TestAppContext) {
        init_theme(cx);
        let calls: Rc<RefCell<Vec<String>>> = Rc::default();
        let factory = fake_factory(calls.clone());
        let (host, cx) = cx.add_window_view(|_, cx| Host {
            view: cx.new(|cx| WebView::with_factory(Some(factory), cx)),
        });
        cx.run_until_parked();
        // Made on its first paint, hidden until its owner says it may show.
        assert_eq!(
            take_calls(&calls),
            ["create", "bounds 1920x1080", "visible false"]
        );
        cx.simulate_resize(size(px(800.), px(600.)));
        cx.run_until_parked();
        assert_eq!(take_calls(&calls), ["bounds 800x600"]);

        let view = host.read_with(cx, |host, _| host.view.clone());
        view.update(cx, |view, cx| {
            view.load_url("http://127.0.0.1:4321/doc/html/index.html", cx)
        });
        view.update(cx, |view, cx| view.set_shown(true, cx));
        assert_eq!(
            take_calls(&calls),
            [
                "load http://127.0.0.1:4321/doc/html/index.html",
                "visible true"
            ]
        );

        // It follows the element as it's laid out again.
        cx.simulate_resize(size(px(640.), px(480.)));
        cx.run_until_parked();
        assert_eq!(take_calls(&calls), ["bounds 640x480"]);

        view.update(cx, |view, cx| view.set_shown(false, cx));
        view.read_with(cx, |view, _| {
            view.back();
            view.reload();
        });
        assert_eq!(
            take_calls(&calls),
            ["visible false", "script history.back()", "reload"]
        );

        // What the page reports reaches the view.
        let events = view.read_with(cx, |view, _| view.events_for_test().unwrap());
        events
            .unbounded_send(NativeEvent::TitleChanged("R: Arithmetic Mean".into()))
            .unwrap();
        events
            .unbounded_send(NativeEvent::Loaded(
                "http://127.0.0.1:4321/library/base/html/mean.html".into(),
            ))
            .unwrap();
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.title(), Some("R: Arithmetic Mean"));
            assert_eq!(
                view.url(),
                Some("http://127.0.0.1:4321/library/base/html/mean.html")
            );
        });

        // A local file is served from its own folder.
        view.update(cx, |view, cx| {
            view.load_file(Path::new("/tmp/Rtmp/viewhtml/index.html"), cx)
        });
        assert_eq!(
            take_calls(&calls),
            ["load rezfile://localhost/tmp/Rtmp/viewhtml/index.html"]
        );
        let root = view.read_with(cx, |view, _| view.file_root.lock().unwrap().clone());
        assert_eq!(root.as_deref(), Some(Path::new("/tmp/Rtmp/viewhtml")));

        // Dropping it hides it straight away.
        let replacement = cx.new(|cx| WebView::with_factory(None, cx));
        host.update(cx, |host, cx| {
            host.view = replacement;
            cx.notify();
        });
        drop(view);
        // GPUI keeps the last frames, whose paint closures hold the old view.
        for height in [470., 460.] {
            cx.simulate_resize(size(px(640.), px(height)));
            cx.run_until_parked();
        }
        assert!(take_calls(&calls).contains(&"visible false".to_string()));
    }

    #[gpui::test]
    fn test_without_a_web_view(cx: &mut TestAppContext) {
        init_theme(cx);
        let (host, cx) = cx.add_window_view(|_, cx| Host {
            view: cx.new(|cx| WebView::with_factory(None, cx)),
        });
        cx.run_until_parked();
        host.read_with(cx, |host, cx| assert!(!host.view.read(cx).is_available()));
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

    impl EventEmitter<DismissEvent> for Dialog {}

    impl workspace::ModalView for Dialog {}

    struct Watcher {
        covered: Vec<bool>,
        _subscriptions: Vec<Subscription>,
    }

    #[gpui::test]
    async fn test_cover_follows_modals(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let fs = project::FakeFs::new(cx.executor());
        let project = project::Project::test(fs, [] as [&Path; 0], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
        let watcher = cx.update(|window, cx| {
            cx.new(|cx| {
                let subscriptions = watch_cover(
                    &workspace,
                    EntityId::from(u64::MAX),
                    window,
                    cx,
                    |this: &mut Watcher, covered, _| this.covered.push(covered),
                );
                Watcher {
                    covered: Vec::new(),
                    _subscriptions: subscriptions,
                }
            })
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.toggle_modal(window, cx, |_, cx| Dialog(cx.focus_handle()))
        });
        cx.run_until_parked();
        assert_eq!(
            watcher.read_with(cx, |watcher, _| watcher.covered.last().copied()),
            Some(true)
        );
        workspace.update_in(cx, |workspace, window, cx| workspace.hide_modal(window, cx));
        cx.run_until_parked();
        assert_eq!(
            watcher.read_with(cx, |watcher, _| watcher.covered.last().copied()),
            Some(false)
        );
    }
}
