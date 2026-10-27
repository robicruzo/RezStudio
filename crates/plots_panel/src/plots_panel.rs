//! The Plots pane: the plots R draws, redrawn by R at the pane's exact size,
//! with history like RStudio's.

mod fitting;
mod history;
mod save_dialog;
mod zoom_window;

use anyhow::{Context as _, Result};
use fitting::{PlotImage, Viewport, ViewportChange, decode_plot, describe_render_error};
use gpui::{
    Action, App, AsyncWindowContext, ClipboardItem, Entity, EntityId, EventEmitter, FocusHandle,
    Focusable, Pixels, PromptLevel, Size, Subscription, Task, WeakEntity, Window, actions, px,
};
use history::PlotHistory;
use runtime::comms::{
    PlotEvent, PlotRenderFormat, PlotRenderSettings, PlotResult, PlotSize, RenderParams,
};
use runtime::{RSession, SessionAdded, SessionEvent, SessionRegistry};
use std::path::PathBuf;
use ui::{IconButton, IconName, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

actions!(
    plots_panel,
    [
        /// Opens the Plots pane and moves focus to it.
        ToggleFocus,
        /// Shows the plot drawn before the one shown.
        PreviousPlot,
        /// Shows the plot drawn after the one shown.
        NextPlot,
        /// Removes the plot shown from the history.
        RemovePlot,
        /// Removes every plot from the history.
        ClearPlots,
        /// Copies the plot shown, at the pane's size, as an image.
        CopyPlot,
        /// Saves the plot shown as a PNG, SVG or PDF file at a chosen size.
        SavePlot,
        /// Opens the plot shown in its own window, redrawn to fit as it's resized.
        ZoomPlot,
        /// Closes a plot's zoom window.
        CloseZoomWindow,
    ]
);

const PANEL_KEY: &str = "PlotsPanel";

#[derive(Clone, Copy, Debug, PartialEq, settings::RegisterSetting)]
pub struct PlotsSettings {
    pub dark_mode: settings::DarkModeContent,
}

impl settings::Settings for PlotsSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self {
            dark_mode: content
                .plots
                .as_ref()
                .and_then(|plots| plots.dark_mode)
                .unwrap_or_default(),
        }
    }
}

/// Whether the Plots pane darkens plots now, from its setting and the theme.
pub fn is_dark_mode_on(cx: &App) -> bool {
    use settings::Settings as _;
    PlotsSettings::get_global(cx)
        .dark_mode
        .is_on(!cx.theme().appearance.is_light())
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<PlotsPanel>(window, cx);
        });
    })
    .detach();
}

/// What a plot is saved as; the dialog starts from the last one used.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PlotExport {
    pub format: PlotRenderFormat,
    pub size: PlotSize,
}

pub struct PlotsPanel {
    workspace: WeakEntity<Workspace>,
    workspace_id: EntityId,
    session: Option<Entity<RSession>>,
    history: PlotHistory<PlotImage>,
    viewport: Viewport,
    /// The last format and size a plot was saved with.
    last_export: Option<PlotExport>,
    focus_handle: FocusHandle,
    position: DockPosition,
    /// The last failure, shown under the toolbar.
    error: Option<String>,
    /// Counts renders, so one that finishes late can be told apart.
    render_sequence: u64,
    /// The render of the shown plot being drawn or decoded, so it isn't asked for twice.
    pending_render: Option<(String, PlotRenderSettings)>,
    _render_task: Option<Task<()>>,
    _resize_task: Option<Task<()>>,
    _session_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl PlotsPanel {
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
        let mut subscriptions = Vec::new();
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
            history: PlotHistory::default(),
            viewport: Viewport::default(),
            last_export: None,
            focus_handle: cx.focus_handle(),
            position: DockPosition::Right,
            error: None,
            render_sequence: 0,
            pending_render: None,
            _render_task: None,
            _resize_task: None,
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
        self._session_subscription = Some(cx.subscribe_in(
            &session,
            window,
            |this, _, event, window, cx| this.handle_session_event(event, window, cx),
        ));
        self.session = Some(session);
        self.send_render_settings(cx);
    }

    fn handle_session_event(
        &mut self,
        event: &SessionEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SessionEvent::PlotOpened {
                comm_id,
                pre_render,
                request_id,
            } => {
                // A chunk's plots show under the chunk instead.
                if let (Some(session), Some(request_id)) = (&self.session, request_id)
                    && session.read(cx).is_inline(request_id)
                {
                    return;
                }
                self.history.add(comm_id.clone());
                if let Some(pre_render) = pre_render {
                    self.accept_render(comm_id.clone(), pre_render.clone(), None, cx);
                }
                self.reveal(window, cx);
                self.ensure_render(cx);
                cx.notify();
            }
            SessionEvent::PlotEvent { comm_id, event } if self.history.contains(comm_id) => {
                let pre_render = match event {
                    PlotEvent::Update { pre_render } => {
                        // What was drawn changed, so any image of it is out of date.
                        self.history.invalidate(comm_id);
                        pre_render
                    }
                    PlotEvent::Show { pre_render } => {
                        if self.history.show(comm_id) {
                            self.reveal(window, cx);
                        }
                        pre_render
                    }
                };
                if let Some(pre_render) = pre_render {
                    self.accept_render(comm_id.clone(), pre_render.clone(), None, cx);
                }
                self.ensure_render(cx);
                cx.notify();
            }
            SessionEvent::PlotClosed { comm_id } => {
                self.history.mark_closed(comm_id);
                cx.notify();
            }
            _ => {}
        }
    }

    /// Brings the pane to the front without taking focus from the editor or
    /// console, as RStudio does when a plot is drawn.
    fn reveal(&self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        // Activating the panel updates it, so it can't happen while it's updating.
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.open_panel::<PlotsPanel>(window, cx)
                })
                .log_err();
        });
    }

    /// The settings that draw a plot to fill the pane exactly.
    fn desired_settings(&self) -> Option<PlotRenderSettings> {
        self.viewport.render_settings()
    }

    fn set_viewport(&mut self, size: Size<Pixels>, scale: f32, cx: &mut Context<Self>) {
        match self.viewport.update(size, scale) {
            ViewportChange::Unchanged => {}
            ViewportChange::First => self.apply_viewport(cx),
            ViewportChange::Changed => {
                let timer = cx.background_executor().timer(fitting::RESIZE_DEBOUNCE);
                self._resize_task = Some(cx.spawn(async move |this, cx| {
                    timer.await;
                    this.update(cx, |this, cx| this.apply_viewport(cx))
                        .log_err();
                }));
            }
        }
    }

    fn apply_viewport(&mut self, cx: &mut Context<Self>) {
        self.viewport.settle();
        self.send_render_settings(cx);
        self.ensure_render(cx);
    }

    /// Tells Ark the pane's size, so new plots arrive drawn to fit.
    fn send_render_settings(&mut self, cx: &mut Context<Self>) {
        if let (Some(session), Some(settings)) = (self.session.clone(), self.desired_settings()) {
            session
                .update(cx, |session, _| session.set_plot_render_settings(settings))
                .log_err();
        }
    }

    /// Asks R to redraw the shown plot if its image doesn't fit the pane.
    fn ensure_render(&mut self, cx: &mut Context<Self>) {
        let Some(settings) = self.desired_settings() else {
            return;
        };
        let Some(plot) = self.history.current() else {
            return;
        };
        if !plot.live || plot.rendered == Some(settings) {
            return;
        }
        let comm_id = plot.comm_id.clone();
        if self.pending_render.as_ref() == Some(&(comm_id.clone(), settings)) {
            return;
        }
        let Some(session) = self.session.clone() else {
            return;
        };
        self.pending_render = Some((comm_id.clone(), settings));
        let render = session.update(cx, |session, cx| {
            session.render_plot(
                &comm_id,
                RenderParams {
                    size: Some(settings.size),
                    pixel_ratio: settings.pixel_ratio,
                    format: settings.format,
                },
                cx,
            )
        });
        self._render_task = Some(cx.spawn(async move |this, cx| {
            let result = render.await;
            this.update(cx, |this, cx| match result {
                Ok(result) => this.accept_render(comm_id, result, Some(settings), cx),
                Err(error) => {
                    this.finish_pending(&comm_id, Some(settings));
                    this.error = Some(describe_render_error(&error));
                    cx.notify();
                }
            })
            .log_err();
        }));
    }

    /// Decodes a render in the background and shows it. `requested` is what
    /// was asked for; a pre-render says what it was drawn with itself.
    fn accept_render(
        &mut self,
        comm_id: String,
        result: PlotResult,
        requested: Option<PlotRenderSettings>,
        cx: &mut Context<Self>,
    ) {
        self.render_sequence += 1;
        let sequence = self.render_sequence;
        let settings = requested.or(result.settings);
        let is_current = self
            .history
            .current()
            .is_some_and(|plot| plot.comm_id == comm_id);
        if let Some(settings) = settings
            && is_current
        {
            self.pending_render = Some((comm_id.clone(), settings));
        }
        let decode = decode_plot(result, cx);
        cx.spawn(async move |this, cx| {
            let decoded = decode.await;
            this.update(cx, |this, cx| {
                this.finish_pending(&comm_id, settings);
                match decoded {
                    Ok(image) => {
                        if let Some(replaced) =
                            this.history.set_image(&comm_id, image, settings, sequence)
                        {
                            cx.drop_image(replaced.render, None);
                        }
                        this.error = None;
                    }
                    Err(error) => this.error = Some(describe_render_error(&error)),
                }
                this.ensure_render(cx);
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    fn finish_pending(&mut self, comm_id: &str, settings: Option<PlotRenderSettings>) {
        if self
            .pending_render
            .as_ref()
            .is_some_and(|(pending_id, pending)| {
                pending_id == comm_id && Some(*pending) == settings
            })
        {
            self.pending_render = None;
        }
    }

    fn go(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.history.go(forward) {
            self.ensure_render(cx);
            cx.notify();
        }
    }

    fn previous_plot(&mut self, _: &PreviousPlot, _: &mut Window, cx: &mut Context<Self>) {
        self.go(false, cx);
    }

    fn next_plot(&mut self, _: &NextPlot, _: &mut Window, cx: &mut Context<Self>) {
        self.go(true, cx);
    }

    fn remove_plot(&mut self, _: &RemovePlot, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(plot) = self.history.remove_current() {
            self.forget_plot(plot, cx);
            self.ensure_render(cx);
            cx.notify();
        }
    }

    fn clear_plots(&mut self, _: &ClearPlots, window: &mut Window, cx: &mut Context<Self>) {
        if self.history.is_empty() {
            return;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            "Remove all plots?",
            Some("This removes every plot in the history. It can't be undone."),
            &["Remove All", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await.ok() == Some(0) {
                this.update(cx, |this, cx| {
                    for plot in this.history.clear() {
                        this.forget_plot(plot, cx);
                    }
                    cx.notify();
                })
                .log_err();
            }
        })
        .detach();
    }

    /// Lets R free a removed plot, and the GPU its image.
    fn forget_plot(&mut self, plot: history::Plot<PlotImage>, cx: &mut Context<Self>) {
        if plot.live
            && let Some(session) = &self.session
        {
            session
                .update(cx, |session, _| session.close_comm(&plot.comm_id))
                .log_err();
        }
        if let Some(image) = plot.image {
            cx.drop_image(image.render, None);
        }
    }

    /// Copies the image shown, which R drew at the pane's size and scale.
    fn copy_plot(&mut self, _: &CopyPlot, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(image) = self.history.current().and_then(|plot| plot.image.as_ref()) {
            cx.write_to_clipboard(ClipboardItem::new_image(&image.png));
        }
    }

    fn open_save_dialog(&mut self, _: &SavePlot, window: &mut Window, cx: &mut Context<Self>) {
        let Some(plot) = self.history.current().filter(|plot| plot.live) else {
            return;
        };
        let comm_id = plot.comm_id.clone();
        let export = self.last_export.unwrap_or(PlotExport {
            format: PlotRenderFormat::Png,
            size: self.viewport.settled_size().unwrap_or(PlotSize {
                width: 800,
                height: 600,
            }),
        });
        let panel = cx.entity().downgrade();
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    save_dialog::SavePlotDialog::new(panel, comm_id, export, window, cx)
                })
            })
            .log_err();
    }

    /// Asks where to save, then has R draw the file at `export`'s size.
    pub(crate) fn save_plot(
        &mut self,
        comm_id: String,
        export: PlotExport,
        cx: &mut Context<Self>,
    ) {
        self.last_export = Some(export);
        let Some(session) = self.session.clone() else {
            return;
        };
        let directory = {
            let session = session.read(cx);
            session
                .working_directory()
                .cloned()
                .unwrap_or_else(|| session.project_root().clone())
        };
        let extension = export.format.extension();
        let chosen = cx.prompt_for_new_path(&directory, Some(&format!("Rplot.{extension}")));
        cx.spawn(async move |this, cx| {
            let saved: Result<()> = async {
                let Some(mut path) = chosen.await.context("the save dialog closed")?? else {
                    return Ok(());
                };
                if path.extension().is_none() {
                    path.set_extension(extension);
                }
                let render = session.update(cx, |session, cx| {
                    session.render_plot(
                        &comm_id,
                        RenderParams {
                            size: Some(export.size),
                            // A PNG is saved at exactly the pixels asked for.
                            pixel_ratio: 1.0,
                            format: export.format,
                        },
                        cx,
                    )
                });
                let result = render.await.map_err(|error| {
                    anyhow::anyhow!(describe_export_error(export.format, &error))
                })?;
                write_plot(path, result, cx).await
            }
            .await;
            if let Err(error) = saved {
                this.update(cx, |this, cx| {
                    this.error = Some(format!("{error:#}"));
                    cx.notify();
                })
                .log_err();
            }
        })
        .detach();
    }

    fn zoom_plot(&mut self, _: &ZoomPlot, _: &mut Window, cx: &mut Context<Self>) {
        let (Some(plot), Some(session)) = (self.history.current(), self.session.clone()) else {
            return;
        };
        zoom_window::open(
            session,
            plot.comm_id.clone(),
            plot.image.clone(),
            plot.live,
            cx,
        )
        .log_err();
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_empty = self.history.is_empty();
        let not_live = self.history.current().is_some_and(|plot| !plot.live);
        let has_image = self
            .history
            .current()
            .is_some_and(|plot| plot.image.is_some());
        h_flex()
            .px_2()
            .py_1()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                IconButton::new("previous-plot", IconName::ChevronLeft)
                    .icon_size(IconSize::Small)
                    .disabled(!self.history.can_go(false))
                    .tooltip(Tooltip::text("Previous Plot"))
                    .on_click(cx.listener(|this, _, _, cx| this.go(false, cx))),
            )
            .child(
                IconButton::new("next-plot", IconName::ChevronRight)
                    .icon_size(IconSize::Small)
                    .disabled(!self.history.can_go(true))
                    .tooltip(Tooltip::text("Next Plot"))
                    .on_click(cx.listener(|this, _, _, cx| this.go(true, cx))),
            )
            .children(
                self.history
                    .position_label()
                    .map(|label| Label::new(label).size(LabelSize::Small).color(Color::Muted)),
            )
            .when(not_live, |this| {
                this.child(
                    div()
                        .id("not-live")
                        .tooltip(Tooltip::text(
                            "R has restarted, so this plot can't be redrawn at a new size.",
                        ))
                        .child(
                            Label::new("Not live")
                                .size(LabelSize::Small)
                                .color(Color::Warning),
                        ),
                )
            })
            .child(div().flex_1())
            .child(
                IconButton::new("zoom-plot", IconName::Maximize)
                    .icon_size(IconSize::Small)
                    .disabled(is_empty)
                    .tooltip(Tooltip::text("Zoom"))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.zoom_plot(&ZoomPlot, window, cx)),
                    ),
            )
            .child(
                IconButton::new("copy-plot", IconName::Copy)
                    .icon_size(IconSize::Small)
                    .disabled(!has_image)
                    .tooltip(Tooltip::text("Copy Plot"))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.copy_plot(&CopyPlot, window, cx)),
                    ),
            )
            .child(
                IconButton::new("save-plot", IconName::Download)
                    .icon_size(IconSize::Small)
                    .disabled(is_empty || not_live)
                    .tooltip(Tooltip::text("Save Plot…"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_save_dialog(&SavePlot, window, cx)
                    })),
            )
            .child(
                IconButton::new("remove-plot", IconName::Close)
                    .icon_size(IconSize::Small)
                    .disabled(is_empty)
                    .tooltip(Tooltip::text("Remove Plot"))
                    .on_click(
                        cx.listener(|this, _, window, cx| {
                            this.remove_plot(&RemovePlot, window, cx)
                        }),
                    ),
            )
            .child(
                IconButton::new("clear-plots", IconName::Trash)
                    .icon_size(IconSize::Small)
                    .disabled(is_empty)
                    .tooltip(Tooltip::text("Clear All Plots"))
                    .on_click(
                        cx.listener(|this, _, window, cx| {
                            this.clear_plots(&ClearPlots, window, cx)
                        }),
                    ),
            )
    }
}

/// Like [`describe_render_error`], with a way out for SVG: without the
/// svglite package, Ark falls back to R's own `svg()`, which on macOS needs
/// XQuartz and otherwise writes nothing, so Ark can't find the file.
fn describe_export_error(format: PlotRenderFormat, error: &anyhow::Error) -> String {
    let description = describe_render_error(error);
    if format == PlotRenderFormat::Svg && format!("{error:#}").contains("No such file or directory")
    {
        format!(
            "{description}. Saving as SVG needs the svglite package: run install.packages(\"svglite\") in the console."
        )
    } else {
        description
    }
}

/// Writes R's render of a plot to `path`.
async fn write_plot(path: PathBuf, result: PlotResult, cx: &mut gpui::AsyncApp) -> Result<()> {
    cx.background_spawn(async move {
        let bytes = result.decode()?;
        std::fs::write(&path, bytes)
            .with_context(|| format!("couldn't save the plot to {}", path.display()))
    })
    .await
}

impl Render for PlotsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let image = self
            .history
            .current()
            .and_then(|plot| plot.image.as_ref())
            .map(|image| image.render.clone());
        let placeholder = match (self.history.current(), &image) {
            (None, _) => Some("Plots appear here when R draws them."),
            (Some(_), None) => Some("Drawing…"),
            _ => None,
        };
        v_flex()
            .key_context("PlotsPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::previous_plot))
            .on_action(cx.listener(Self::next_plot))
            .on_action(cx.listener(Self::remove_plot))
            .on_action(cx.listener(Self::clear_plots))
            .on_action(cx.listener(Self::copy_plot))
            .on_action(cx.listener(Self::open_save_dialog))
            .on_action(cx.listener(Self::zoom_plot))
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(fitting::measure(entity, Self::set_viewport))
                    .children(image.map(fitting::plot_image))
                    .children(
                        self.error
                            .clone()
                            .map(|error| fitting::error_overlay(error.into(), cx)),
                    )
                    .children(placeholder.map(fitting::placeholder)),
            )
    }
}

impl Focusable for PlotsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for PlotsPanel {}

impl Panel for PlotsPanel {
    fn persistent_name() -> &'static str {
        "Plots"
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
        Some(IconName::Image)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Plots")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn starts_open(&self, _: &Window, _: &App) -> bool {
        false
    }

    fn activation_priority(&self) -> u32 {
        10
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::channel::mpsc;
    use gpui::{TestAppContext, VisualTestContext, size};
    use jupyter_protocol::{
        CommId, CommMsg, CommOpen, JupyterMessage, JupyterMessageContent, KernelInfoReply,
    };
    use languages::r::{RInstallationSource, RToolchainData};
    use project::{FakeFs, Project};
    use runtime::comms::PLOT_COMM_TARGET;
    use serde_json::{Value, json};
    use settings::SettingsStore;
    use std::path::PathBuf;
    use workspace::MultiWorkspace;

    #[gpui::test]
    fn test_dark_mode_setting(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let set = |mode: settings::DarkModeContent, cx: &mut gpui::TestAppContext| {
            cx.update(|cx| {
                cx.update_global::<SettingsStore, _>(|store, cx| {
                    store.update_user_settings(cx, |settings| {
                        settings.plots = Some(settings::PlotsSettingsContent {
                            dark_mode: Some(mode),
                        });
                    });
                });
                is_dark_mode_on(cx)
            })
        };
        assert!(!cx.update(|cx| is_dark_mode_on(cx)), "off by default");
        assert!(set(settings::DarkModeContent::On, cx));
        assert!(!set(settings::DarkModeContent::Off, cx));
        let theme_is_dark = cx.update(|cx| !cx.theme().appearance.is_light());
        assert_eq!(
            set(settings::DarkModeContent::FollowTheme, cx),
            theme_is_dark
        );
    }

    /// A 1×1 PNG, base64-encoded without padding as Ark sends it.
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg";

    struct Harness {
        workspace: Entity<Workspace>,
        panel: Entity<PlotsPanel>,
        session: Entity<RSession>,
        requests: mpsc::Receiver<JupyterMessage>,
        ui_comm: String,
    }

    #[derive(Debug, PartialEq)]
    struct Request {
        comm_id: String,
        method: String,
        params: Value,
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
                JupyterMessageContent::CommOpen(open) => open.comm_id.0,
                other => panic!("expected the UI comm, got {}", other.message_type()),
            };
            let panel = workspace.update_in(cx, |workspace, window, cx| {
                let panel = cx.new(|cx| {
                    let mut panel = PlotsPanel::new(workspace, window, cx);
                    panel.set_session(session.clone(), window, cx);
                    panel
                });
                workspace.add_panel(panel.clone(), window, cx);
                panel
            });
            let mut harness = Self {
                workspace,
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
            // The rest of startup: `frontend_ready` and the LSP and variables comms.
            while harness.requests.try_recv().is_ok() {}
            (harness, cx)
        }

        fn receive(&self, message: JupyterMessage, cx: &mut VisualTestContext) {
            self.session.update_in(cx, |session, window, cx| {
                session.receive_for_test(&message, window, cx)
            });
            cx.run_until_parked();
        }

        /// Ark sending a new plot, drawn at `drawn_at`.
        fn open_plot(&self, comm_id: &str, drawn_at: Value, cx: &mut VisualTestContext) {
            let Value::Object(data) = json!({ "pre_render": pre_render(drawn_at) }) else {
                unreachable!()
            };
            self.receive(
                JupyterMessage::new(
                    CommOpen {
                        comm_id: CommId(comm_id.to_string()),
                        target_name: PLOT_COMM_TARGET.to_string(),
                        data,
                        target_module: None,
                    },
                    None,
                ),
                cx,
            );
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

        fn drain(&mut self) -> Vec<(Request, JupyterMessage)> {
            std::iter::from_fn(|| self.requests.try_recv().ok())
                .map(|message| {
                    let request = match &message.content {
                        JupyterMessageContent::CommMsg(comm) => Request {
                            comm_id: comm.comm_id.0.clone(),
                            method: comm.data["method"].as_str().unwrap_or_default().to_string(),
                            params: comm.data.get("params").cloned().unwrap_or_default(),
                        },
                        JupyterMessageContent::CommClose(close) => Request {
                            comm_id: close.comm_id.0.clone(),
                            method: "comm_close".to_string(),
                            params: Value::Null,
                        },
                        other => panic!("unexpected {}", other.message_type()),
                    };
                    (request, message)
                })
                .collect()
        }

        fn requests(&mut self) -> Vec<Request> {
            self.drain()
                .into_iter()
                .map(|(request, _)| request)
                .collect()
        }

        /// Takes the requests sent, expecting the pane's settings and then
        /// one render of `comm_id` at that size, which is answered.
        fn answer_render(&mut self, comm_id: &str, cx: &mut VisualTestContext) {
            let drawn_at = self.drawn_at(cx);
            let mut requests = self.drain();
            let (render, message) = requests.pop().expect("a render");
            assert_eq!(render.comm_id, comm_id);
            assert_eq!(render.method, "render");
            assert_eq!(
                render.params,
                json!({ "size": drawn_at["size"], "pixel_ratio": drawn_at["pixel_ratio"], "format": "png" })
            );
            for (request, _) in requests {
                assert_eq!(
                    request,
                    Request {
                        comm_id: self.ui_comm.clone(),
                        method: "did_change_plots_render_settings".into(),
                        params: json!({ "settings": drawn_at }),
                    }
                );
            }
            let Value::Object(data) = json!({ "method": "render", "result": pre_render(drawn_at) })
            else {
                unreachable!()
            };
            self.receive(
                JupyterMessage::new(
                    CommMsg {
                        comm_id: CommId(comm_id.to_string()),
                        data,
                    },
                    Some(&message),
                ),
                cx,
            );
        }

        fn reply(&self, request: &JupyterMessage, data: Value, cx: &mut VisualTestContext) {
            let Value::Object(data) = data else {
                panic!("reply data must be an object");
            };
            let JupyterMessageContent::CommMsg(comm) = &request.content else {
                panic!("expected a comm request");
            };
            self.receive(
                JupyterMessage::new(
                    CommMsg {
                        comm_id: comm.comm_id.clone(),
                        data,
                    },
                    Some(request),
                ),
                cx,
            );
        }

        /// The settings that fill the pane as it's laid out now.
        fn drawn_at(&self, cx: &mut VisualTestContext) -> Value {
            let settings = self
                .panel
                .read_with(cx, |panel, _| panel.desired_settings())
                .expect("the pane is laid out");
            serde_json::to_value(settings).unwrap()
        }

        fn current(&self, cx: &mut VisualTestContext) -> (Option<String>, Option<String>) {
            self.panel.read_with(cx, |panel, _| {
                (
                    panel.history.current().map(|plot| plot.comm_id.clone()),
                    panel.history.position_label(),
                )
            })
        }

        fn resize(&self, height: f32, cx: &mut VisualTestContext) {
            cx.simulate_resize(size(px(1200.), px(height)));
            cx.run_until_parked();
        }

        fn settle(&self, cx: &mut VisualTestContext) {
            cx.executor().advance_clock(fitting::RESIZE_DEBOUNCE * 2);
            cx.run_until_parked();
        }
    }

    fn pre_render(drawn_at: Value) -> Value {
        json!({ "data": PNG, "mime_type": "image/png", "settings": drawn_at })
    }

    fn ark_default() -> Value {
        json!({ "size": { "width": 640, "height": 400 }, "pixel_ratio": 1.0, "format": "png" })
    }

    #[gpui::test]
    async fn test_new_plot_comes_to_front_and_is_redrawn_to_fit(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        let right_dock = harness
            .workspace
            .read_with(cx, |workspace, _| workspace.right_dock().clone());
        assert!(!right_dock.read_with(cx, |dock, _| dock.is_open()));

        // Ark drew the plot at its default size before knowing the pane's.
        harness.open_plot("plot-1", ark_default(), cx);
        right_dock.read_with(cx, |dock, _| {
            assert!(dock.is_open());
            assert_eq!(
                dock.visible_panel().map(|panel| panel.panel_id()),
                Some(harness.panel.entity_id())
            );
        });
        let focused = harness.panel.update_in(cx, |panel, window, cx| {
            panel.focus_handle(cx).contains_focused(window, cx)
        });
        assert!(!focused, "drawing a plot doesn't take focus");

        harness.answer_render("plot-1", cx);
        assert!(harness.requests().is_empty(), "drawn once");
        harness.panel.read_with(cx, |panel, _| {
            let plot = panel.history.current().unwrap();
            assert!(plot.image.is_some());
            assert_eq!(plot.rendered, panel.desired_settings());
            assert_eq!(panel.error, None);
        });

        // The next plot arrives drawn to fit, so nothing is redrawn.
        let drawn_at = harness.drawn_at(cx);
        harness.open_plot("plot-2", drawn_at, cx);
        assert!(harness.requests().is_empty());
        assert_eq!(
            harness.current(cx),
            (Some("plot-2".into()), Some("2 / 2".into()))
        );
    }

    #[gpui::test]
    async fn test_resizing_navigating_and_removing(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        harness.open_plot("plot-1", ark_default(), cx);
        harness.answer_render("plot-1", cx);
        let drawn_at = harness.drawn_at(cx);
        harness.open_plot("plot-2", drawn_at.clone(), cx);

        // Only the size the pane settles at is drawn.
        harness.resize(700., cx);
        harness.resize(650., cx);
        assert!(
            harness.requests().is_empty(),
            "nothing until resizing stops"
        );
        harness.settle(cx);
        assert_ne!(harness.drawn_at(cx), drawn_at);
        harness.answer_render("plot-2", cx);
        assert!(harness.requests().is_empty());

        // Going back redraws the older plot at the new size.
        harness.panel.update(cx, |panel, cx| panel.go(false, cx));
        cx.run_until_parked();
        assert_eq!(harness.current(cx).1.as_deref(), Some("1 / 2"));
        harness.answer_render("plot-1", cx);

        // More drawn on a plot draws it again; `show` brings a plot forward.
        harness.event(
            "plot-1",
            json!({ "method": "update", "params": { "pre_render": null } }),
            cx,
        );
        harness.answer_render("plot-1", cx);
        harness.event("plot-2", json!({ "method": "show", "params": {} }), cx);
        assert_eq!(harness.current(cx).0.as_deref(), Some("plot-2"));
        assert!(harness.requests().is_empty(), "plot-2 already fits");

        // Removing a plot lets R free it.
        harness.panel.update_in(cx, |panel, window, cx| {
            panel.remove_plot(&RemovePlot, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            harness.requests(),
            [Request {
                comm_id: "plot-2".into(),
                method: "comm_close".into(),
                params: Value::Null
            }]
        );
        assert_eq!(
            harness.current(cx),
            (Some("plot-1".into()), Some("1 / 1".into()))
        );

        harness.panel.update_in(cx, |panel, window, cx| {
            panel.clear_plots(&ClearPlots, window, cx)
        });
        cx.run_until_parked();
        cx.simulate_prompt_answer("Remove All");
        cx.run_until_parked();
        assert_eq!(harness.requests()[0].comm_id, "plot-1");
        assert_eq!(harness.current(cx), (None, None));
    }

    #[gpui::test]
    async fn test_resizing_the_dock_redraws(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        harness.open_plot("plot-1", ark_default(), cx);
        harness.answer_render("plot-1", cx);
        let before = harness.drawn_at(cx);

        harness.workspace.update_in(cx, |workspace, window, cx| {
            workspace.set_panel_size_state::<PlotsPanel>(
                workspace::dock::PanelSizeState {
                    size: Some(px(520.)),
                    flex: None,
                },
                window,
                cx,
            );
        });
        cx.run_until_parked();
        harness.settle(cx);
        assert_ne!(
            harness.drawn_at(cx),
            before,
            "the pane measured its new width"
        );
        harness.answer_render("plot-1", cx);
        assert!(harness.requests().is_empty());
    }

    #[gpui::test]
    async fn test_dragging_the_dock_edge_redraws(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        harness.resize(800., cx);
        harness.open_plot("plot-1", ark_default(), cx);
        harness.answer_render("plot-1", cx);
        let width = harness.drawn_at(cx)["size"]["width"].as_f64().unwrap() as f32;

        // Drag the dock's left edge 100 pixels further left.
        let edge = gpui::point(px(1200. - width - 1.), px(400.));
        cx.simulate_mouse_down(edge, gpui::MouseButton::Left, Default::default());
        for step in 1..=10 {
            cx.simulate_mouse_move(
                gpui::point(edge.x - px(10. * step as f32), edge.y),
                gpui::MouseButton::Left,
                Default::default(),
            );
        }
        cx.simulate_mouse_up(
            gpui::point(edge.x - px(100.), edge.y),
            gpui::MouseButton::Left,
            Default::default(),
        );
        cx.run_until_parked();
        harness.settle(cx);
        let new_width = harness.drawn_at(cx)["size"]["width"].as_f64().unwrap() as f32;
        assert!(
            (new_width - width - 100.).abs() <= 2.,
            "the pane is {new_width} wide after dragging from {width}"
        );
        harness.answer_render("plot-1", cx);
        assert!(harness.requests().is_empty());
    }

    #[gpui::test]
    async fn test_draws_only_settled_sizes_and_survives_failures(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        harness.open_plot("plot-1", ark_default(), cx);
        let first_size = harness.drawn_at(cx);
        let (render, message) = harness.drain().pop().expect("a render");
        assert_eq!(render.method, "render");

        // A render that lands mid-resize doesn't start another at a passing size.
        harness.resize(700., cx);
        harness.reply(
            &message,
            json!({ "method": "render", "result": pre_render(first_size) }),
            cx,
        );
        assert!(
            harness.requests().is_empty(),
            "nothing drawn until resizing stops"
        );
        harness.settle(cx);
        harness.answer_render("plot-1", cx);

        // When R can't draw at a size, the last image stays and one line says why,
        // over the plot so the pane's size doesn't change.
        harness.resize(650., cx);
        harness.settle(cx);
        let viewport = harness
            .panel
            .read_with(cx, |panel, _| panel.viewport.current());
        let (_, message) = harness.drain().pop().expect("a render");
        harness.reply(
            &message,
            json!({ "error": { "code": -32603, "message": "Failed to process positron.plot request: Error evaluating 'value[[3L]](cond)': invalid graphics state\n\nR backtrace:\n0. tryCatchOne(expr, names, parentenv, handlers[[1L]])" } }),
            cx,
        );
        harness.panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.error.as_deref(),
                Some("R couldn't draw the plot: invalid graphics state")
            );
            assert!(panel.history.current().unwrap().image.is_some());
            assert_eq!(panel.viewport.current(), viewport);
        });
        harness.resize(600., cx);
        harness.settle(cx);
        harness.answer_render("plot-1", cx);
        assert_eq!(
            harness.panel.read_with(cx, |panel, _| panel.error.clone()),
            None
        );

        // Too small to draw: the last image is shown scaled instead.
        harness.resize(150., cx);
        harness.settle(cx);
        assert!(harness.requests().is_empty());
    }

    #[gpui::test]
    async fn test_copying_and_saving(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        harness.open_plot("plot-1", ark_default(), cx);
        harness.answer_render("plot-1", cx);

        // Copy puts the PNG R drew for the pane on the clipboard.
        harness.panel.update_in(cx, |panel, window, cx| {
            panel.copy_plot(&CopyPlot, window, cx)
        });
        let copied = cx.read_from_clipboard().expect("something copied");
        let Some(gpui::ClipboardEntry::Image(image)) = copied.entries().first() else {
            panic!("expected an image, got {copied:?}");
        };
        assert_eq!(image.format, gpui::ImageFormat::Png);
        assert_eq!(
            image.bytes,
            PlotResult {
                data: PNG.into(),
                mime_type: "image/png".into(),
                settings: None
            }
            .decode()
            .unwrap()
        );

        // Save asks for a format and size, then where, then has R draw the file.
        let dialog = |harness: &Harness, cx: &mut VisualTestContext| {
            harness.workspace.read_with(cx, |workspace, cx| {
                workspace.active_modal::<save_dialog::SavePlotDialog>(cx)
            })
        };
        harness.panel.update_in(cx, |panel, window, cx| {
            panel.open_save_dialog(&SavePlot, window, cx)
        });
        let save = dialog(&harness, cx).expect("the save dialog");
        save.update_in(cx, |dialog, window, cx| {
            dialog.set_for_test(PlotRenderFormat::Svg, "800", "abc", window, cx);
            dialog.confirm(&menu::Confirm, window, cx);
        });
        assert!(
            save.read_with(cx, |dialog, _| dialog.error_for_test())
                .is_some(),
            "a bad height is reported"
        );
        save.update_in(cx, |dialog, window, cx| {
            dialog.set_for_test(PlotRenderFormat::Svg, "800", " 600 ", window, cx);
            dialog.confirm(&menu::Confirm, window, cx);
        });
        cx.run_until_parked();
        assert!(dialog(&harness, cx).is_none(), "the dialog closes");
        let directory = tempfile::tempdir().unwrap();
        let chosen = directory.path().join("chart");
        cx.simulate_new_path_selection(|_| Some(chosen.clone()));
        cx.run_until_parked();
        let (render, message) = harness.drain().pop().expect("a render for the file");
        assert_eq!(
            render.params,
            json!({ "size": { "width": 800, "height": 600 }, "pixel_ratio": 1.0, "format": "svg" })
        );
        harness.reply(
            &message,
            json!({ "method": "render", "result": {
                "data": "PHN2Zz4", "mime_type": "image/svg+xml", "settings": null
            }}),
            cx,
        );
        cx.run_until_parked();
        assert_eq!(
            std::fs::read_to_string(directory.path().join("chart.svg")).unwrap(),
            "<svg>"
        );

        // The dialog starts from what was used last.
        harness.panel.update_in(cx, |panel, window, cx| {
            panel.open_save_dialog(&SavePlot, window, cx)
        });
        let save = dialog(&harness, cx).expect("the save dialog");
        save.read_with(cx, |dialog, cx| {
            assert_eq!(
                dialog.values_for_test(cx),
                (PlotRenderFormat::Svg, "800".into(), "600".into())
            );
        });
    }

    #[gpui::test]
    async fn test_zoom_window_redraws_to_fit(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        harness.open_plot("plot-1", ark_default(), cx);
        harness.answer_render("plot-1", cx);

        harness.panel.update_in(cx, |panel, window, cx| {
            panel.zoom_plot(&ZoomPlot, window, cx)
        });
        cx.run_until_parked();
        let zoom = cx
            .windows()
            .into_iter()
            .find_map(|window| window.downcast::<zoom_window::PlotZoomWindow>())
            .expect("a zoom window");
        let requests = harness.drain();
        let methods: Vec<&str> = requests
            .iter()
            .map(|(request, _)| request.method.as_str())
            .collect();
        assert_eq!(methods, ["get_metadata", "render"]);
        let (render, message) = &requests[1];
        assert_eq!(
            render.params["size"],
            json!({ "width": 800, "height": 600 })
        );
        let zoom_size = render.params.clone();
        harness.reply(
            &requests[0].1,
            json!({ "method": "get_metadata", "result": {
                "name": "plot 1", "kind": "base", "execution_id": "", "code": "plot(1:10)"
            }}),
            cx,
        );
        harness.reply(
            message,
            json!({ "method": "render", "result": pre_render(json!({
            "size": zoom_size["size"], "pixel_ratio": zoom_size["pixel_ratio"], "format": "png"
        })) }),
            cx,
        );
        cx.run_until_parked();
        assert!(harness.requests().is_empty(), "drawn once");
        zoom.read_with(cx, |zoom, _| assert!(zoom.has_own_image_for_test()))
            .unwrap();

        // More drawn on the plot redraws it in the window as well as the pane.
        harness.event(
            "plot-1",
            json!({ "method": "update", "params": { "pre_render": null } }),
            cx,
        );
        let sizes: Vec<Value> = harness
            .requests()
            .into_iter()
            .filter(|request| request.method == "render")
            .map(|request| request.params["size"].clone())
            .collect();
        assert!(
            sizes.contains(&json!({ "width": 800, "height": 600 })),
            "{sizes:?}"
        );
        assert_eq!(sizes.len(), 2, "{sizes:?}");

        // Escape closes it.
        zoom.update(cx, |_, window, cx| {
            window.dispatch_action(Box::new(CloseZoomWindow), cx)
        })
        .unwrap();
        cx.run_until_parked();
        assert!(
            cx.windows()
                .iter()
                .all(|window| window.downcast::<zoom_window::PlotZoomWindow>().is_none())
        );
    }

    #[test]
    fn test_export_errors_explain_missing_svglite() {
        let missing = anyhow::anyhow!(
            "Failed to process positron.plot request: No such file or directory (os error 2)"
        );
        let message = describe_export_error(PlotRenderFormat::Svg, &missing);
        assert!(
            message.starts_with("R couldn't draw the plot: No such file or directory"),
            "{message}"
        );
        assert!(
            message.contains("install.packages(\"svglite\")"),
            "{message}"
        );
        assert!(!describe_export_error(PlotRenderFormat::Pdf, &missing).contains("svglite"));
    }

    #[gpui::test]
    async fn test_chunk_plots_stay_out_of_the_pane(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        let right_dock = harness
            .workspace
            .read_with(cx, |workspace, _| workspace.right_dock().clone());
        harness
            .session
            .update(cx, |session, cx| {
                session.execute_inline("plot(1)".into(), cx)
            })
            .unwrap();
        harness.requests.try_recv().unwrap();

        harness.open_plot("chunk-plot", ark_default(), cx);
        harness.event(
            "chunk-plot",
            json!({ "method": "update", "params": { "pre_render": null } }),
            cx,
        );
        assert_eq!(harness.current(cx), (None, None));
        assert!(
            !right_dock.read_with(cx, |dock, _| dock.is_open()),
            "the pane stays put"
        );
        assert!(harness.requests().is_empty(), "nothing is drawn for it");
    }

    #[gpui::test]
    async fn test_plots_outlive_r(cx: &mut TestAppContext) {
        let (mut harness, cx) = Harness::new(cx).await;
        harness.open_plot("plot-1", ark_default(), cx);
        harness.answer_render("plot-1", cx);

        harness
            .session
            .update(cx, |session, cx| session.stop(cx))
            .detach();
        cx.run_until_parked();
        // Ark's shutdown request.
        while harness.requests.try_recv().is_ok() {}
        harness.panel.read_with(cx, |panel, _| {
            let plot = panel.history.current().unwrap();
            assert!(!plot.live);
            assert!(plot.image.is_some(), "its last image is kept");
        });

        // It can't be redrawn or freed, but can still be removed.
        harness.resize(600., cx);
        harness.settle(cx);
        harness.panel.update_in(cx, |panel, window, cx| {
            panel.remove_plot(&RemovePlot, window, cx)
        });
        cx.run_until_parked();
        assert!(harness.requests().is_empty());
        assert_eq!(harness.current(cx), (None, None));
    }
}
