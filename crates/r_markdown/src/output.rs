//! What a chunk produced, shown under it: printed output, tables and errors
//! through Zed's REPL output view, then the chunk's plots, which R redraws
//! at the editor's width.

use anyhow::Result;
use gpui::{
    App, Entity, Image, ImageFormat, ImageSource, ObjectFit, Pixels, RenderImage, Task, WeakEntity,
    Window, canvas, img, px,
};
use jupyter_protocol::JupyterMessageContent;
use repl::{ExecutionStatus, ExecutionView};
use runtime::RSession;
use runtime::comms::{PlotRenderFormat, PlotSize, RenderParams};
use std::sync::Arc;
use std::time::Duration;
use ui::prelude::*;
use util::ResultExt as _;
use workspace::Workspace;

/// R draws plots at the width the output area settles at.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(150);
/// Wider than this, a plot's text gets too small to read on a wide screen.
const MAX_PLOT_WIDTH: f32 = 900.;
/// Narrower than this, R can't fit a plot's margins.
const MIN_PLOT_WIDTH: f32 = 200.;
/// knitr's defaults, in inches: `fig.width = 7`, `fig.height = 5`.
const DEFAULT_ASPECT: f32 = 5. / 7.;

#[derive(Clone, Debug, PartialEq)]
pub enum RunState {
    Queued,
    Running,
    Finished {
        succeeded: bool,
    },
    /// Not run, with why, as when an earlier chunk in a Run All failed.
    Skipped(String),
}

struct InlinePlot {
    comm_id: String,
    image: Option<Arc<RenderImage>>,
    /// The width it was last asked for at, so it's redrawn only when that changes.
    requested: Option<(i64, f32)>,
    error: Option<String>,
    _render: Option<Task<()>>,
}

pub struct ChunkOutput {
    session: Entity<RSession>,
    workspace: WeakEntity<Workspace>,
    execution: Entity<ExecutionView>,
    state: RunState,
    plots: Vec<InlinePlot>,
    /// Plot height over width, from the chunk's figure options.
    aspect: f32,
    /// The output area's width and the screen's scale, once it stopped changing.
    width: Option<(Pixels, f32)>,
    measured: Option<(Pixels, f32)>,
    _resize: Option<Task<()>>,
}

impl ChunkOutput {
    pub fn new(
        session: Entity<RSession>,
        workspace: WeakEntity<Workspace>,
        aspect: Option<f32>,
        cx: &mut Context<Self>,
    ) -> Self {
        let execution =
            cx.new(|cx| ExecutionView::new(ExecutionStatus::Queued, workspace.clone(), cx));
        Self {
            session,
            workspace,
            execution,
            state: RunState::Queued,
            plots: Vec::new(),
            aspect: aspect.unwrap_or(DEFAULT_ASPECT),
            width: None,
            measured: None,
            _resize: None,
        }
    }

    #[cfg(test)]
    pub fn state(&self) -> &RunState {
        &self.state
    }

    pub fn plot_ids(&self) -> impl Iterator<Item = &str> {
        self.plots.iter().map(|plot| plot.comm_id.as_str())
    }

    /// Starts over for a new run, dropping the last run's output and letting R
    /// free its plots.
    pub fn reset(&mut self, aspect: Option<f32>, cx: &mut Context<Self>) {
        self.aspect = aspect.unwrap_or(DEFAULT_ASPECT);
        self.forget_plots(cx);
        let workspace = self.workspace.clone();
        self.execution = cx.new(|cx| ExecutionView::new(ExecutionStatus::Queued, workspace, cx));
        self.set_state(RunState::Queued, cx);
    }

    pub fn set_state(&mut self, state: RunState, cx: &mut Context<Self>) {
        let status = match &state {
            RunState::Queued => ExecutionStatus::Queued,
            RunState::Running => ExecutionStatus::Executing,
            RunState::Finished { .. } | RunState::Skipped(_) => ExecutionStatus::Finished,
        };
        self.execution.update(cx, |execution, cx| {
            execution.status = status;
            cx.notify();
        });
        self.state = state;
        cx.notify();
    }

    pub fn push(
        &mut self,
        content: &JupyterMessageContent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The code itself is in the editor, above.
        if matches!(content, JupyterMessageContent::ExecuteInput(_)) {
            return;
        }
        self.execution.update(cx, |execution, cx| {
            execution.push_message(content, window, cx)
        });
        cx.notify();
    }

    pub fn add_plot(&mut self, comm_id: String, cx: &mut Context<Self>) {
        self.plots.push(InlinePlot {
            comm_id,
            image: None,
            requested: None,
            error: None,
            _render: None,
        });
        self.render_plots(cx);
        cx.notify();
    }

    /// More was drawn on a plot, as by `lines()`, so it's drawn again.
    pub fn plot_changed(&mut self, comm_id: &str, cx: &mut Context<Self>) {
        if let Some(plot) = self.plots.iter_mut().find(|plot| plot.comm_id == comm_id) {
            plot.requested = None;
        }
        self.render_plots(cx);
    }

    /// Lets R free the plots and the GPU their images, as when the output goes away.
    pub fn forget_plots(&mut self, cx: &mut Context<Self>) {
        for plot in std::mem::take(&mut self.plots) {
            self.session
                .update(cx, |session, _| session.close_comm(&plot.comm_id))
                .ok();
            if let Some(image) = plot.image {
                cx.drop_image(image, None);
            }
        }
    }

    fn plot_size(&self) -> Option<(PlotSize, f32)> {
        let (width, scale) = self.width?;
        let width = f32::from(width).min(MAX_PLOT_WIDTH);
        if width < MIN_PLOT_WIDTH {
            return None;
        }
        Some((
            PlotSize {
                width: width.floor() as i64,
                height: (width * self.aspect).floor() as i64,
            },
            scale,
        ))
    }

    fn render_plots(&mut self, cx: &mut Context<Self>) {
        let Some((size, scale)) = self.plot_size() else {
            return;
        };
        for index in 0..self.plots.len() {
            let Some(plot) = self.plots.get_mut(index) else {
                continue;
            };
            if plot.requested == Some((size.width, scale)) {
                continue;
            }
            plot.requested = Some((size.width, scale));
            let comm_id = plot.comm_id.clone();
            let render = self.session.update(cx, |session, cx| {
                session.render_plot(
                    &comm_id,
                    RenderParams {
                        size: Some(size),
                        pixel_ratio: f64::from(scale),
                        format: PlotRenderFormat::Png,
                    },
                    cx,
                )
            });
            let svg_renderer = cx.svg_renderer();
            let task = cx.spawn(async move |this, cx| {
                let decoded: Result<Arc<RenderImage>> = async {
                    let result = render.await?;
                    cx.background_spawn(async move {
                        Image::from_bytes(ImageFormat::Png, result.decode()?)
                            .to_image_data(svg_renderer)
                    })
                    .await
                }
                .await;
                this.update(cx, |this, cx| {
                    let Some(plot) = this.plots.iter_mut().find(|plot| plot.comm_id == comm_id)
                    else {
                        return;
                    };
                    match decoded {
                        Ok(image) => {
                            if let Some(replaced) = plot.image.replace(image) {
                                cx.drop_image(replaced, None);
                            }
                            plot.error = None;
                        }
                        Err(error) => {
                            let message = format!("{error:#}");
                            plot.error =
                                Some(message.lines().next().unwrap_or_default().to_string());
                        }
                    }
                    cx.notify();
                })
                .log_err();
            });
            if let Some(plot) = self.plots.get_mut(index) {
                plot._render = Some(task);
            }
        }
    }

    fn set_measured_width(&mut self, width: Pixels, scale: f32, cx: &mut Context<Self>) {
        if self.measured == Some((width, scale)) {
            return;
        }
        let first = self.measured.is_none();
        self.measured = Some((width, scale));
        if first {
            self.width = self.measured;
            self.render_plots(cx);
            return;
        }
        let timer = cx.background_executor().timer(RESIZE_DEBOUNCE);
        self._resize = Some(cx.spawn(async move |this, cx| {
            timer.await;
            this.update(cx, |this, cx| {
                this.width = this.measured;
                this.render_plots(cx);
                cx.notify();
            })
            .log_err();
        }));
    }
}

impl Render for ChunkOutput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let height = self
            .plot_size()
            .map(|(size, _)| px(size.height as f32))
            .unwrap_or(px(240.));
        let width = self
            .plot_size()
            .map(|(size, _)| px(size.width as f32))
            .unwrap_or(px(320.));
        let skipped = match &self.state {
            RunState::Skipped(reason) => Some(reason.clone()),
            _ => None,
        };
        v_flex()
            .w_full()
            .gap_2()
            .child(
                // Reports the area's width, which plots are drawn to fill.
                div().relative().w_full().h(px(0.)).child(
                    canvas(
                        |_, _, _| (),
                        move |bounds, _, window, cx| {
                            let scale = window.scale_factor();
                            entity.update(cx, |this, cx| {
                                this.set_measured_width(bounds.size.width, scale, cx)
                            });
                        },
                    )
                    .absolute()
                    .size_full(),
                ),
            )
            .children(skipped.map(|reason| Label::new(reason).color(Color::Muted)))
            // A skipped chunk has no output, and the view's finished tick would mislead.
            .when(!matches!(self.state, RunState::Skipped(_)), |this| {
                this.child(self.execution.clone())
            })
            .children(self.plots.iter().map(|plot| {
                match (&plot.image, &plot.error) {
                    (Some(image), _) => div()
                        .w(width)
                        .h(height)
                        .child(
                            img(ImageSource::Render(image.clone()))
                                .size_full()
                                .object_fit(ObjectFit::Contain),
                        )
                        .into_any_element(),
                    (None, Some(error)) => Label::new(format!("R couldn't draw the plot: {error}"))
                        .color(Color::Error)
                        .into_any_element(),
                    (None, None) => div()
                        .w(width)
                        .h(height)
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(cx.theme().colors().editor_background)
                        .child(Label::new("Drawing…").color(Color::Muted))
                        .into_any_element(),
                }
            }))
    }
}

/// The plot's height over its width, from `fig.width`/`fig.height` (R
/// Markdown) or `fig-width`/`fig-height` (Quarto), in any units.
pub fn aspect_from_options(option: impl Fn(&str) -> Option<String>) -> Option<f32> {
    let number = |keys: [&str; 2]| {
        keys.iter()
            .find_map(|key| option(key))
            .and_then(|value| value.trim().parse::<f32>().ok())
            .filter(|value| *value > 0.)
    };
    let width = number(["fig.width", "fig-width"]).unwrap_or(7.);
    let height = number(["fig.height", "fig-height"]).unwrap_or(5.);
    let given = option("fig.width").or(option("fig-width")).is_some()
        || option("fig.height").or(option("fig-height")).is_some();
    given.then_some(height / width)
}

#[cfg(test)]
impl ChunkOutput {
    pub fn plot_images(&self) -> usize {
        self.plots
            .iter()
            .filter(|plot| plot.image.is_some())
            .count()
    }

    pub fn output_count(&self, cx: &App) -> usize {
        self.execution.read(cx).outputs.len()
    }
}

pub fn drop_output(output: &Entity<ChunkOutput>, cx: &mut App) {
    output.update(cx, |output, cx| output.forget_plots(cx));
}
