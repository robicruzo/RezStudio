//! The Render tab: renders Quarto and R Markdown documents in a process of
//! their own, as RStudio does, with the output streaming here and the result
//! shown in the Viewer.

mod command;

use anyhow::Result;
use collections::{HashMap, HashSet};
use command::{
    OutputLine, RenderCommand, parse_line, render_command, shows_in_viewer, strip_escapes,
};
use editor::{Editor, EditorEvent};
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{
    Action, App, AsyncWindowContext, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    Global, ListHorizontalSizingBehavior, Pixels, ScrollStrategy, Subscription, Task,
    UniformListScrollHandle, WeakEntity, Window, actions, px, uniform_list,
};
use runtime::SessionRegistry;
use smol::io::{AsyncBufReadExt as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use ui::{IconButton, IconName, Tooltip, prelude::*};
use util::ResultExt as _;
use viewer_panel::ViewerPanel;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};
use workspace::item::{Item as _, SaveOptions};

pub use command::DocumentKind;

actions!(
    render_panel,
    [
        /// Opens the Render tab and moves focus to it.
        ToggleFocus,
        /// Renders the Quarto or R Markdown document being edited.
        Render,
        /// Stops the render that's running.
        StopRender,
        /// Turns rendering the document each time it's saved on or off.
        ToggleRenderOnSave,
    ]
);

const PANEL_KEY: &str = "RenderPanel";
/// Older output is dropped beyond this many lines.
const MAX_LINES: usize = 5000;

pub fn init(cx: &mut App) {
    cx.set_global(RenderOnSave::default());
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<RenderPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &Render, window, cx| {
            if let Some(editor) = workspace.active_item_as::<Editor>(cx) {
                render_editor(workspace, editor, window, cx);
            }
        });
        workspace.register_action(|workspace, _: &StopRender, _, cx| {
            if let Some(panel) = workspace.panel::<RenderPanel>(cx) {
                panel.update(cx, |panel, cx| panel.stop(cx));
            }
        });
        workspace.register_action(|workspace, _: &ToggleRenderOnSave, window, cx| {
            if let Some(editor) = workspace.active_item_as::<Editor>(cx) {
                toggle_render_on_save(&editor, window, cx);
            }
        });
    })
    .detach();
}

/// The file an editor would render: a Quarto or R Markdown file on disk.
pub fn renderable_path(editor: &Entity<Editor>, cx: &App) -> Option<PathBuf> {
    let buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
    let path = buffer.read(cx).file()?.as_local()?.abs_path(cx);
    DocumentKind::of(&path)?;
    Some(path)
}

/// Renders the document in `editor`, saving it first, as RStudio does.
pub fn render_editor(
    workspace: &mut Workspace,
    editor: Entity<Editor>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(path) = renderable_path(&editor, cx) else {
        return;
    };
    let Some(panel) = workspace.panel::<RenderPanel>(cx) else {
        return;
    };
    if !editor.read(cx).is_dirty(cx) {
        panel.update(cx, |panel, cx| panel.render_document(path, window, cx));
        return;
    }
    let editor_id = editor.entity_id();
    // The save would render again with Render on Save on.
    cx.update_global(|state: &mut RenderOnSave, _| state.saving_to_render.insert(editor_id));
    let project = workspace.project().clone();
    let save = editor.update(cx, |editor, cx| {
        editor.save(
            SaveOptions {
                format: true,
                force_format: false,
                autosave: false,
            },
            project,
            window,
            cx,
        )
    });
    cx.spawn_in(window, async move |_, cx| {
        let saved = save.await;
        cx.update(|_, cx| {
            cx.update_global(|state: &mut RenderOnSave, _| {
                state.saving_to_render.remove(&editor_id)
            })
        })
        .log_err();
        panel
            .update_in(cx, |panel, window, cx| match saved {
                Ok(()) => panel.render_document(path, window, cx),
                Err(error) => panel.fail(format!("Couldn't save the document: {error:#}"), cx),
            })
            .log_err();
    })
    .detach();
}

/// The editors that render on each save, as with RStudio's Render on Save.
#[derive(Default)]
struct RenderOnSave {
    editors: HashMap<EntityId, Subscription>,
    /// Editors that Render is saving, whose save shouldn't render again.
    saving_to_render: HashSet<EntityId>,
}

impl Global for RenderOnSave {}

pub fn is_render_on_save(editor: &Entity<Editor>, cx: &App) -> bool {
    cx.try_global::<RenderOnSave>()
        .is_some_and(|state| state.editors.contains_key(&editor.entity_id()))
}

pub fn toggle_render_on_save(editor: &Entity<Editor>, window: &mut Window, cx: &mut App) {
    let editor_id = editor.entity_id();
    if is_render_on_save(editor, cx) {
        cx.update_global(|state: &mut RenderOnSave, _| state.editors.remove(&editor_id));
        return;
    }
    let subscription = window.subscribe(editor, cx, |editor, event: &EditorEvent, window, cx| {
        if matches!(event, EditorEvent::Saved) {
            render_saved(&editor, window, cx);
        }
    });
    cx.update_global(|state: &mut RenderOnSave, _| state.editors.insert(editor_id, subscription));
    cx.observe_release(editor, move |_, cx| {
        cx.update_global(|state: &mut RenderOnSave, _| state.editors.remove(&editor_id));
    })
    .detach();
}

/// Calls `callback` when Render on Save is turned on or off for any editor.
pub fn observe_render_on_save<T: 'static>(
    cx: &mut Context<T>,
    callback: impl FnMut(&mut T, &mut Context<T>) + 'static,
) -> Subscription {
    cx.observe_global::<RenderOnSave>(callback)
}

fn render_saved(editor: &Entity<Editor>, window: &mut Window, cx: &mut App) {
    if cx
        .try_global::<RenderOnSave>()
        .is_some_and(|state| state.saving_to_render.contains(&editor.entity_id()))
    {
        return;
    }
    let Some(path) = renderable_path(editor, cx) else {
        return;
    };
    let Some(panel) = editor
        .read(cx)
        .workspace()
        .and_then(|workspace| workspace.read(cx).panel::<RenderPanel>(cx))
    else {
        return;
    };
    panel.update(cx, |panel, cx| panel.render_document(path, window, cx));
}

#[derive(Clone, Debug, PartialEq)]
enum Status {
    Idle,
    Rendering,
    /// `quarto preview` is serving the result here.
    Serving(String),
    Rendered,
    Stopped,
    Failed(String),
}

struct Job {
    /// Distinguishes this render's output from an earlier one's still arriving.
    id: usize,
    document: PathBuf,
    kind: DocumentKind,
    process: util::process::Child,
    /// The file the renderer said it wrote.
    created: Option<PathBuf>,
    stopping: bool,
    finished: bool,
    _task: Task<()>,
}

impl Drop for Job {
    fn drop(&mut self) {
        // `quarto preview` keeps serving until stopped, and its children run
        // in its process group, which this kills.
        if !self.finished {
            self.process.kill().log_err();
        }
    }
}

pub struct RenderPanel {
    workspace: WeakEntity<Workspace>,
    workspace_id: EntityId,
    job: Option<Job>,
    next_job_id: usize,
    /// The document of the last render.
    document: Option<PathBuf>,
    status: Status,
    lines: Vec<SharedString>,
    scroll_handle: UniformListScrollHandle,
    focus_handle: FocusHandle,
    position: DockPosition,
    _quit_subscription: Subscription,
}

impl RenderPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, _, cx| {
            cx.new(|cx| Self::new(workspace, cx))
        })
    }

    pub fn new(workspace: &mut Workspace, cx: &mut Context<Self>) -> Self {
        let workspace_handle = workspace.weak_handle();
        Self {
            workspace_id: workspace_handle.entity_id(),
            workspace: workspace_handle,
            job: None,
            next_job_id: 0,
            document: None,
            status: Status::Idle,
            lines: Vec::new(),
            scroll_handle: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            position: DockPosition::Bottom,
            // A `quarto preview` server would otherwise outlive the app.
            _quit_subscription: cx.on_app_quit(|this, _| {
                this.job = None;
                async {}
            }),
        }
    }

    pub fn is_rendering(&self) -> bool {
        self.job.is_some()
    }

    /// Renders `document`, stopping any render still running, and shows the
    /// Render tab.
    pub fn render_document(
        &mut self,
        document: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start(document.clone(), window, cx);

        // R's environment: the project's shell environment and its R.
        let session = SessionRegistry::global(cx)
            .and_then(|registry| registry.read(cx).session(self.workspace_id).cloned());
        let (env, r_home) = match &session {
            Some(session) => {
                let session = session.read(cx);
                (
                    session.environment().clone(),
                    session.r_installation().map(|r| r.r_home.clone()),
                )
            }
            None => (std::env::vars().collect(), None),
        };
        let command =
            match render_command(&document, &env, r_home.as_deref(), util::paths::home_dir()) {
                Ok(command) => command,
                Err(error) => {
                    self.fail(format!("{error:#}"), cx);
                    return;
                }
            };
        self.run(document, command, window, cx);
    }

    /// Stops the render running, clears its output, and shows the Render tab.
    fn start(&mut self, document: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        // Dropping the job stops its process.
        self.job = None;
        self.lines.clear();
        self.document = Some(document);
        self.reveal(window, cx);
    }

    /// Runs `command`, which renders `document`.
    fn run(
        &mut self,
        document: PathBuf,
        command: RenderCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.push_text(format!(
            "$ {} {}",
            command.program.display(),
            command.args.join(" ")
        ));
        let kind = DocumentKind::of(&document).unwrap_or(DocumentKind::Quarto);
        let mut std_command = util::command::new_std_command(&command.program);
        std_command
            .args(&command.args)
            .current_dir(&command.directory)
            .env_clear()
            .envs(&command.env);
        let mut process = match util::process::Child::spawn(
            std_command,
            Stdio::null(),
            Stdio::piped(),
            Stdio::piped(),
        ) {
            Ok(process) => process,
            Err(error) => {
                self.fail(format!("Couldn't start rendering: {error:#}"), cx);
                return;
            }
        };

        let (line_sender, mut lines) = mpsc::unbounded::<String>();
        if let Some(stdout) = process.stdout.take() {
            cx.background_spawn(forward_lines(stdout, line_sender.clone()))
                .detach();
        }
        if let Some(stderr) = process.stderr.take() {
            cx.background_spawn(forward_lines(stderr, line_sender))
                .detach();
        }
        let id = self.next_job_id;
        self.next_job_id += 1;
        let task = cx.spawn_in(window, async move |this, cx| {
            while let Some(line) = lines.next().await {
                if this
                    .update_in(cx, |this, window, cx| {
                        this.handle_line(id, line, window, cx)
                    })
                    .is_err()
                {
                    return;
                }
            }
            // The output ends when the renderer and everything it started exit.
            let Ok(Some(status)) = this.update(cx, |this, _| {
                this.job
                    .as_mut()
                    .filter(|job| job.id == id)
                    .map(|job| job.process.status())
            }) else {
                return;
            };
            let status = status.await;
            this.update_in(cx, |this, window, cx| this.finish(id, status, window, cx))
                .log_err();
        });
        self.job = Some(Job {
            id,
            document,
            kind,
            process,
            created: None,
            stopping: false,
            finished: false,
            _task: task,
        });
        self.status = Status::Rendering;
        cx.notify();
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(job) = self.job.as_mut()
            && !job.stopping
        {
            job.stopping = true;
            job.process.kill().log_err();
            cx.notify();
        }
    }

    fn fail(&mut self, message: String, cx: &mut Context<Self>) {
        self.push_text(message.clone());
        self.status = Status::Failed(message);
        cx.notify();
    }

    fn push_text(&mut self, line: String) {
        self.lines.push(line.into());
        if self.lines.len() > MAX_LINES {
            let excess = self.lines.len() - MAX_LINES;
            self.lines.drain(..excess);
        }
        if let Some(last) = self.lines.len().checked_sub(1) {
            self.scroll_handle
                .scroll_to_item(last, ScrollStrategy::Bottom);
        }
    }

    fn handle_line(
        &mut self,
        id: usize,
        line: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(job) = self.job.as_mut().filter(|job| job.id == id) else {
            return;
        };
        let line = strip_escapes(&line);
        match parse_line(&line) {
            OutputLine::Created(file) => {
                let directory = job.document.parent().unwrap_or(Path::new("/"));
                job.created = Some(directory.join(file));
            }
            OutputLine::Browse(url) if job.kind == DocumentKind::Quarto => {
                let document = job.document.clone();
                let created = job.created.clone();
                self.status = Status::Serving(url.clone());
                if shows_in_viewer(&url) {
                    self.show_url(document, url, window, cx);
                } else if let Some(created) = created {
                    cx.open_with_system(&created);
                }
            }
            OutputLine::Browse(_) | OutputLine::Other => {}
        }
        self.push_text(line);
        cx.notify();
    }

    fn finish(
        &mut self,
        id: usize,
        status: std::io::Result<std::process::ExitStatus>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mut job) = self.job.take_if(|job| job.id == id) else {
            return;
        };
        job.finished = true;
        self.status = if job.stopping {
            Status::Stopped
        } else {
            match status {
                Ok(status) if status.success() => Status::Rendered,
                Ok(status) => Status::Failed(match status.code() {
                    Some(code) => format!("Rendering failed with exit code {code}."),
                    None => "Rendering was stopped.".to_string(),
                }),
                Err(error) => Status::Failed(format!("Rendering failed: {error}")),
            }
        };
        if self.status == Status::Rendered
            && job.kind == DocumentKind::RMarkdown
            && let Some(created) = job.created.clone()
        {
            if shows_in_viewer(&created.to_string_lossy()) {
                self.show_file(created, window, cx);
            } else {
                cx.open_with_system(&created);
            }
        }
        if let Status::Failed(message) = &self.status {
            let message = message.clone();
            self.push_text(message);
        }
        cx.notify();
    }

    fn viewer(&self, cx: &App) -> Option<Entity<ViewerPanel>> {
        self.workspace.upgrade()?.read(cx).panel::<ViewerPanel>(cx)
    }

    fn show_url(
        &mut self,
        document: PathBuf,
        url: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.viewer(cx) {
            Some(viewer) => viewer.update(cx, |viewer, cx| {
                viewer.show_rendered_url(document, url, window, cx)
            }),
            None => cx.open_url(&url),
        }
    }

    fn show_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        match self.viewer(cx) {
            Some(viewer) => {
                viewer.update(cx, |viewer, cx| viewer.show_rendered_file(path, window, cx))
            }
            None => cx.open_with_system(&path),
        }
    }

    fn reveal(&self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        // Opening a panel updates the docks, which can't happen mid-update.
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.open_panel::<RenderPanel>(window, cx)
                })
                .log_err();
        });
    }

    fn status_label(&self) -> (String, Color) {
        let name = self
            .document
            .as_ref()
            .and_then(|document| document.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        match &self.status {
            Status::Idle => (
                "Render a Quarto or R Markdown document to see its output here.".to_string(),
                Color::Muted,
            ),
            Status::Rendering => (format!("Rendering {name}…"), Color::Default),
            Status::Serving(url) => (format!("Previewing {name} at {url}"), Color::Default),
            Status::Rendered => (format!("Rendered {name}"), Color::Success),
            Status::Stopped => (format!("Stopped rendering {name}"), Color::Muted),
            Status::Failed(_) => (format!("Couldn't render {name}"), Color::Error),
        }
    }
}

/// Sends each line `stream` writes until it closes.
async fn forward_lines(
    stream: impl smol::io::AsyncRead + Unpin,
    sender: mpsc::UnboundedSender<String>,
) {
    let mut lines = BufReader::new(stream).lines();
    while let Some(Ok(line)) = lines.next().await {
        if sender.unbounded_send(line).is_err() {
            break;
        }
    }
}

impl gpui::Render for RenderPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (status, color) = self.status_label();
        let running = self.job.as_ref().is_some_and(|job| !job.stopping);
        let line_count = self.lines.len();
        v_flex()
            .key_context("RenderPanel")
            .track_focus(&self.focus_handle)
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
                        div().flex_1().min_w_0().child(
                            Label::new(status)
                                .size(LabelSize::Small)
                                .color(color)
                                .truncate(),
                        ),
                    )
                    .when(running, |this| {
                        this.child(
                            IconButton::new("stop-render", IconName::Stop)
                                .icon_size(IconSize::Small)
                                .icon_color(Color::Error)
                                .tooltip(Tooltip::text("Stop Rendering"))
                                .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                        )
                    })
                    .child(
                        IconButton::new("clear-render-output", IconName::Trash)
                            .icon_size(IconSize::Small)
                            .disabled(self.lines.is_empty() || running)
                            .tooltip(Tooltip::text("Clear Output"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.lines.clear();
                                cx.notify();
                            })),
                    ),
            )
            .child(
                uniform_list(
                    "render-output",
                    line_count,
                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .filter_map(|index| {
                                let line = this.lines.get(index)?.clone();
                                // Rows are all one line high, so a long line
                                // scrolls sideways rather than wrapping over the
                                // rows below it.
                                Some(
                                    div()
                                        .px_2()
                                        .whitespace_nowrap()
                                        .child(
                                            Label::new(line)
                                                .size(LabelSize::Small)
                                                .buffer_font(cx)
                                                .single_line(),
                                        )
                                        .into_any_element(),
                                )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.scroll_handle)
                .with_width_from_item(widest_line(&self.lines))
                .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
                .flex_1(),
            )
    }
}

fn widest_line(lines: &[SharedString]) -> Option<usize> {
    lines
        .iter()
        .enumerate()
        .max_by_key(|(_, line)| line.chars().count())
        .map(|(index, _)| index)
}

impl Focusable for RenderPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for RenderPanel {}

impl Panel for RenderPanel {
    fn persistent_name() -> &'static str {
        "Render"
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
        px(300.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::Book)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Render")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        14
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;
    use web_view::{WebView, fake_factory, take_calls};
    use workspace::MultiWorkspace;

    struct Harness {
        panel: Entity<RenderPanel>,
        workspace: Entity<Workspace>,
        /// What the Viewer's web view was asked to do.
        web_view_calls: Rc<RefCell<Vec<String>>>,
    }

    async fn new_panel(cx: &mut TestAppContext) -> (Harness, &mut VisualTestContext) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            serde_json::json!({ "report.qmd": "# Report\n", "notes.md": "# Notes\n" }),
        )
        .await;
        let project = Project::test(fs, [Path::new("/project")], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
        let web_view_calls: Rc<RefCell<Vec<String>>> = Rc::default();
        let web_view =
            cx.new(|cx| WebView::with_factory(Some(fake_factory(web_view_calls.clone())), cx));
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let viewer = cx.new(|cx| ViewerPanel::new(workspace, web_view, window, cx));
            workspace.add_panel(viewer, window, cx);
            let panel = cx.new(|cx| RenderPanel::new(workspace, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        let harness = Harness {
            panel,
            workspace,
            web_view_calls,
        };
        (harness, cx)
    }

    fn loads(harness: &Harness) -> Vec<String> {
        take_calls(&harness.web_view_calls)
            .into_iter()
            .filter(|call| call.starts_with("load "))
            .collect()
    }

    /// Runs a shell script in place of the renderer.
    fn run_script(
        panel: &Entity<RenderPanel>,
        document: &str,
        script: &str,
        cx: &mut VisualTestContext,
    ) {
        let document = std::env::temp_dir().join(document);
        let command = RenderCommand {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".to_string(), script.to_string()],
            env: [("PATH".to_string(), "/usr/bin:/bin".to_string())]
                .into_iter()
                .collect(),
            directory: std::env::temp_dir(),
        };
        panel.update_in(cx, |panel, window, cx| {
            panel.start(document.clone(), window, cx);
            panel.run(document, command, window, cx);
        });
    }

    fn wait_for(
        panel: &Entity<RenderPanel>,
        cx: &mut VisualTestContext,
        condition: impl Fn(&RenderPanel) -> bool,
    ) {
        wait_for_within(Duration::from_secs(10), panel, cx, condition)
    }

    fn wait_for_within(
        timeout: Duration,
        panel: &Entity<RenderPanel>,
        cx: &mut VisualTestContext,
        condition: impl Fn(&RenderPanel) -> bool,
    ) {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            cx.run_until_parked();
            if panel.read_with(cx, |panel, _| condition(panel)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let (status, lines) =
            panel.read_with(cx, |panel, _| (panel.status.clone(), panel.lines.clone()));
        panic!("timed out with status {status:?} and output {lines:?}");
    }

    #[test]
    fn test_widest_line() {
        assert_eq!(widest_line(&[]), None);
        let lines: Vec<SharedString> =
            vec!["short".into(), "the longest line".into(), "mid".into()];
        assert_eq!(widest_line(&lines), Some(1));
    }

    fn has_line(panel: &RenderPanel, text: &str) -> bool {
        panel.lines.iter().any(|line| line.as_ref() == text)
    }

    #[gpui::test]
    async fn test_quarto_preview_serves_until_stopped(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (harness, cx) = new_panel(cx).await;
        let panel = harness.panel.clone();
        run_script(
            &panel,
            "report.qmd",
            "printf '\\033[32mprocessing file: report.qmd\\033[39m\\n'; \
             echo 'Output created: report.html'; \
             echo 'Browse at http://localhost:1234/report.html'; \
             exec sleep 30",
            cx,
        );
        wait_for(&panel, cx, |panel| {
            panel.status == Status::Serving("http://localhost:1234/report.html".to_string())
        });
        panel.read_with(cx, |panel, _| {
            assert!(
                has_line(panel, "processing file: report.qmd"),
                "{:?}",
                panel.lines
            );
            assert!(panel.is_rendering());
        });
        assert_eq!(loads(&harness), ["load http://localhost:1234/report.html"]);

        panel.update(cx, |panel, cx| panel.stop(cx));
        wait_for(&panel, cx, |panel| panel.status == Status::Stopped);
        assert!(!panel.read_with(cx, |panel, _| panel.is_rendering()));
    }

    #[gpui::test]
    async fn test_r_markdown_render_finishes(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (harness, cx) = new_panel(cx).await;
        let panel = harness.panel.clone();
        run_script(
            &panel,
            "notes.Rmd",
            "echo 'processing file: notes.Rmd'; echo 'Output created: notes.html'",
            cx,
        );
        wait_for(&panel, cx, |panel| panel.status == Status::Rendered);
        panel.read_with(cx, |panel, _| {
            assert!(has_line(panel, "Output created: notes.html"));
            assert!(!panel.is_rendering());
        });
        let html = std::env::temp_dir().join("notes.html");
        assert_eq!(
            loads(&harness),
            [format!("load {}", web_view::file_url(&html))]
        );
    }

    #[gpui::test]
    async fn test_failure_shows_the_exit_code(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (harness, cx) = new_panel(cx).await;
        let panel = harness.panel.clone();
        run_script(
            &panel,
            "notes.Rmd",
            "echo 'Error: object not found' >&2; exit 3",
            cx,
        );
        wait_for(&panel, cx, |panel| {
            matches!(panel.status, Status::Failed(_))
        });
        panel.read_with(cx, |panel, _| {
            assert!(
                has_line(panel, "Error: object not found"),
                "{:?}",
                panel.lines
            );
            assert!(has_line(panel, "Rendering failed with exit code 3."));
        });
        assert!(loads(&harness).is_empty(), "a failed render shows nothing");
    }

    #[gpui::test]
    async fn test_a_new_render_replaces_the_running_one(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (harness, cx) = new_panel(cx).await;
        let panel = harness.panel;
        run_script(&panel, "report.qmd", "echo first; exec sleep 30", cx);
        wait_for(&panel, cx, |panel| has_line(panel, "first"));
        run_script(&panel, "notes.Rmd", "echo second", cx);
        wait_for(&panel, cx, |panel| panel.status == Status::Rendered);
        panel.read_with(cx, |panel, _| {
            assert!(!has_line(panel, "first"), "{:?}", panel.lines);
            assert!(has_line(panel, "second"));
        });
    }

    #[gpui::test]
    async fn test_render_on_save_toggles_for_quarto_files(cx: &mut TestAppContext) {
        let (harness, cx) = new_panel(cx).await;
        let workspace = harness.workspace;
        let mut editors = Vec::new();
        for path in ["/project/report.qmd", "/project/notes.md"] {
            let item = workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.open_abs_path(PathBuf::from(path), Default::default(), window, cx)
                })
                .await
                .unwrap();
            editors.push(item.downcast::<Editor>().unwrap());
        }
        let (report, notes) = (editors[0].clone(), editors[1].clone());
        cx.update(|_, cx| {
            assert_eq!(
                renderable_path(&report, cx),
                Some(PathBuf::from("/project/report.qmd"))
            );
            assert_eq!(renderable_path(&notes, cx), None);
        });

        cx.update(|window, cx| toggle_render_on_save(&report, window, cx));
        assert!(cx.update(|_, cx| is_render_on_save(&report, cx)));
        assert!(!cx.update(|_, cx| is_render_on_save(&notes, cx)));
        cx.update(|window, cx| toggle_render_on_save(&report, window, cx));
        assert!(!cx.update(|_, cx| is_render_on_save(&report, cx)));
    }

    /// A page's body, fetched over plain HTTP from a server on this machine.
    fn http_get(url: &str) -> String {
        use std::io::{Read as _, Write as _};

        let rest = url.strip_prefix("http://").unwrap();
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let mut stream = std::net::TcpStream::connect(authority).unwrap();
        stream
            .write_all(format!("GET /{path} HTTP/1.0\r\nHost: {authority}\r\n\r\n").as_bytes())
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    /// Renders a Quarto and an R Markdown document with the real tools,
    /// through the commands Render builds. Needs R with rmarkdown, and
    /// Quarto: `cargo test -p render_panel -- --ignored`.
    #[gpui::test]
    #[ignore = "needs R, rmarkdown and Quarto"]
    #[allow(clippy::disallowed_methods)]
    async fn test_real_render(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let (harness, cx) = new_panel(cx).await;
        let panel = harness.panel.clone();
        let folder = tempfile::tempdir().unwrap();
        let r_home = std::process::Command::new("R")
            .arg("RHOME")
            .output()
            .expect("R must be on PATH");
        let r_home = PathBuf::from(String::from_utf8(r_home.stdout).unwrap().trim());
        let env: HashMap<String, String> = std::env::vars().collect();
        let chunk = "```{r}\ncat('The answer is', 40 + 2)\n```\n";

        let report = folder.path().join("report.qmd");
        std::fs::write(&report, format!("---\ntitle: Report\n---\n\n{chunk}")).unwrap();
        let command =
            render_command(&report, &env, Some(&r_home), util::paths::home_dir()).unwrap();
        panel.update_in(cx, |panel, window, cx| {
            panel.start(report.clone(), window, cx);
            panel.run(report.clone(), command, window, cx);
        });
        wait_for_within(Duration::from_secs(120), &panel, cx, |panel| {
            matches!(panel.status, Status::Serving(_) | Status::Failed(_))
        });
        let Status::Serving(url) = panel.read_with(cx, |panel, _| panel.status.clone()) else {
            panic!("{:?}", panel.read_with(cx, |panel, _| panel.lines.clone()));
        };
        assert_eq!(loads(&harness), [format!("load {url}")]);
        let page = http_get(&url);
        assert!(page.contains("The answer is 42"), "{page:.2000}");
        panel.update(cx, |panel, cx| panel.stop(cx));
        wait_for(&panel, cx, |panel| panel.status == Status::Stopped);

        let notes = folder.path().join("notes.Rmd");
        std::fs::write(
            &notes,
            format!("---\ntitle: Notes\noutput: html_document\n---\n\n{chunk}"),
        )
        .unwrap();
        let command = render_command(&notes, &env, Some(&r_home), util::paths::home_dir()).unwrap();
        panel.update_in(cx, |panel, window, cx| {
            panel.start(notes.clone(), window, cx);
            panel.run(notes.clone(), command, window, cx);
        });
        wait_for_within(Duration::from_secs(120), &panel, cx, |panel| {
            matches!(panel.status, Status::Rendered | Status::Failed(_))
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.status, Status::Rendered, "{:?}", panel.lines);
        });
        let html = folder.path().join("notes.html");
        assert_eq!(
            loads(&harness),
            [format!("load {}", web_view::file_url(&html))]
        );
        let page = std::fs::read_to_string(&html).unwrap();
        assert!(page.contains("The answer is 42"), "{page:.2000}");
    }
}
