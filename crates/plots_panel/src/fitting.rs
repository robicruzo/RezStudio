//! Drawing a plot to fill an area exactly, for the Plots pane and the zoom
//! window: the area's size as laid out, settled once it stops changing, and
//! R's renders decoded off the main thread.

use anyhow::Result;
use gpui::{
    App, Entity, Image, ImageFormat, ImageSource, ObjectFit, Pixels, RenderImage, Size, Task,
    canvas, img,
};
use runtime::comms::{PlotRenderFormat, PlotRenderSettings, PlotResult, PlotSize};
use std::sync::Arc;
use std::time::Duration;
use ui::{Tooltip, prelude::*};

/// Resizing changes the size every frame, and Zed animates docks; R redraws
/// once the size settles.
pub(crate) const RESIZE_DEBOUNCE: Duration = Duration::from_millis(150);
/// Smaller than this, R often can't fit a plot's margins and fails with
/// "figure margins too large" or "invalid graphics state".
const MIN_PLOT_SIZE: i64 = 120;

/// A render decoded and ready to paint, with its PNG for the clipboard.
#[derive(Clone)]
pub(crate) struct PlotImage {
    pub render: Arc<RenderImage>,
    pub png: Arc<Image>,
}

pub(crate) fn decode_plot(result: PlotResult, cx: &App) -> Task<Result<PlotImage>> {
    let svg_renderer = cx.svg_renderer();
    cx.background_spawn(async move {
        anyhow::ensure!(
            result.mime_type == "image/png",
            "R sent a plot as {}, not PNG",
            result.mime_type
        );
        let png = Arc::new(Image::from_bytes(ImageFormat::Png, result.decode()?));
        let render = png.to_image_data(svg_renderer)?;
        Ok(PlotImage { render, png })
    })
}

#[derive(Debug, PartialEq)]
pub(crate) enum ViewportChange {
    Unchanged,
    /// The first layout, which is drawn at once rather than after settling.
    First,
    Changed,
}

/// The size a plot area is laid out at, in logical pixels, and the screen's scale.
#[derive(Default)]
pub(crate) struct Viewport {
    current: Option<(PlotSize, f32)>,
    /// `current` once it stopped changing. Plots are only drawn at this size:
    /// mid-drag sizes are passing, and very narrow ones make R fail.
    settled: Option<(PlotSize, f32)>,
}

impl Viewport {
    pub fn update(&mut self, size: Size<Pixels>, scale: f32) -> ViewportChange {
        let size = PlotSize {
            width: f32::from(size.width).floor() as i64,
            height: f32::from(size.height).floor() as i64,
        };
        if self.current == Some((size, scale)) {
            return ViewportChange::Unchanged;
        }
        let first = self.current.is_none();
        self.current = Some((size, scale));
        if first {
            ViewportChange::First
        } else {
            ViewportChange::Changed
        }
    }

    pub fn settle(&mut self) {
        self.settled = self.current;
    }

    #[cfg(test)]
    pub fn current(&self) -> Option<(PlotSize, f32)> {
        self.current
    }

    /// The size the area settled at, in logical pixels.
    pub fn settled_size(&self) -> Option<PlotSize> {
        self.settled.map(|(size, _)| size)
    }

    /// The settings that draw a plot to fill the area, once it has settled at
    /// a size R can draw in.
    pub fn render_settings(&self) -> Option<PlotRenderSettings> {
        let (size, scale) = self.settled?;
        (size.width >= MIN_PLOT_SIZE && size.height >= MIN_PLOT_SIZE).then_some(
            PlotRenderSettings {
                size,
                pixel_ratio: f64::from(scale),
                format: PlotRenderFormat::Png,
            },
        )
    }
}

/// An invisible element filling its parent that reports the parent's size to
/// `on_layout` whenever it's painted.
pub(crate) fn measure<V: 'static>(
    entity: Entity<V>,
    on_layout: fn(&mut V, Size<Pixels>, f32, &mut Context<V>),
) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, cx| {
            let scale = window.scale_factor();
            entity.update(cx, |view, cx| on_layout(view, bounds.size, scale, cx));
        },
    )
    .absolute()
    .size_full()
}

pub(crate) fn plot_image(render: Arc<RenderImage>) -> impl IntoElement {
    div().absolute().size_full().child(
        img(ImageSource::Render(render))
            .size_full()
            .object_fit(ObjectFit::Contain),
    )
}

/// One line over the bottom of the plot, so an error never changes the size
/// plots are drawn at.
pub(crate) fn error_overlay(error: SharedString, cx: &App) -> impl IntoElement {
    div()
        .id("plot-error")
        .absolute()
        .bottom_0()
        .left_0()
        .right_0()
        .px_2()
        .py_1()
        .bg(cx.theme().colors().panel_background)
        .border_t_1()
        .border_color(cx.theme().colors().border_variant)
        .tooltip(Tooltip::text(error.clone()))
        .child(
            Label::new(error)
                .size(LabelSize::Small)
                .color(Color::Error)
                .single_line()
                .truncate(),
        )
}

pub(crate) fn placeholder(text: &'static str) -> impl IntoElement {
    div()
        .absolute()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .p_4()
        .child(Label::new(text).size(LabelSize::Small).color(Color::Muted))
}

/// One line from a failed render: Ark's errors carry R's message, wrapped in
/// Ark's own context and followed by R and Rust backtraces.
pub(crate) fn describe_render_error(error: &anyhow::Error) -> String {
    let message = format!("{error:#}");
    let first_line = message.lines().next().unwrap_or_default().trim();
    let message = first_line
        .strip_prefix("Failed to process positron.plot request: ")
        .unwrap_or(first_line);
    let message = match message.strip_prefix("Error evaluating '") {
        Some(rest) => rest.split_once("': ").map_or(message, |(_, reason)| reason),
        None => message,
    };
    format!("R couldn't draw the plot: {message}")
}
