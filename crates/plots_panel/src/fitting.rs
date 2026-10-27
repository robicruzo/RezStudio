//! Drawing a plot to fill an area exactly, for the Plots pane and the zoom
//! window: the area's size as laid out, settled once it stops changing, and
//! R's renders decoded off the main thread.

use anyhow::{Context as _, Result};
use gpui::{
    App, Entity, Hsla, Image, ImageFormat, ImageSource, ObjectFit, Pixels, RenderImage, Rgba, Size,
    SvgRenderer, Task, canvas, img, rgb,
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

/// A render decoded and ready to paint, with R's PNG for the clipboard.
#[derive(Clone)]
pub(crate) struct PlotImage {
    pub render: Arc<RenderImage>,
    pub png: Arc<Image>,
    /// How `render` was darkened from the PNG, if it was.
    pub darkening: Option<Darkening>,
}

pub(crate) fn decode_plot(
    result: PlotResult,
    darkening: Option<Darkening>,
    cx: &App,
) -> Task<Result<PlotImage>> {
    let svg_renderer = cx.svg_renderer();
    cx.background_spawn(async move {
        anyhow::ensure!(
            result.mime_type == "image/png",
            "R sent a plot as {}, not PNG",
            result.mime_type
        );
        let png = Arc::new(Image::from_bytes(ImageFormat::Png, result.decode()?));
        plot_image_from_png(png, darkening, svg_renderer)
    })
}

/// Decodes a plot's PNG again, as when dark mode is turned on or off, so R
/// needn't draw it again.
pub(crate) fn redraw_plot(
    png: Arc<Image>,
    darkening: Option<Darkening>,
    cx: &App,
) -> Task<Result<PlotImage>> {
    let svg_renderer = cx.svg_renderer();
    cx.background_spawn(async move { plot_image_from_png(png, darkening, svg_renderer) })
}

fn plot_image_from_png(
    png: Arc<Image>,
    darkening: Option<Darkening>,
    svg_renderer: SvgRenderer,
) -> Result<PlotImage> {
    let render = png.to_image_data(svg_renderer)?;
    let render = match darkening {
        Some(darkening) => darkening.darken_render(&render)?,
        None => render,
    };
    Ok(PlotImage {
        render,
        png,
        darkening,
    })
}

/// How a plot is darkened: each pixel's lightness is flipped while its hue
/// and saturation stay, then mapped so R's white becomes `background` and
/// its black `foreground`. Only the image shown is darkened.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Darkening {
    pub background: Rgba,
    pub foreground: Rgba,
}

impl Darkening {
    /// How plots shown on `background` are darkened now, or `None` when the
    /// Plots pane's dark mode is off. With a light theme and dark mode on,
    /// plots go dark gray rather than taking the theme's light colors.
    pub fn current(background: Hsla, cx: &App) -> Option<Self> {
        if !crate::is_dark_mode_on(cx) {
            return None;
        }
        let theme = cx.theme();
        Some(if theme.appearance.is_light() {
            Self {
                background: rgb(0x1e1e1e),
                foreground: rgb(0xd4d4d4),
            }
        } else {
            Self {
                background: background.into(),
                foreground: theme.colors().text.into(),
            }
        })
    }

    fn darken_render(&self, render: &RenderImage) -> Result<Arc<RenderImage>> {
        let frames = (0..render.frame_count())
            .map(|index| {
                let size = render.size(index);
                let pixels = render
                    .as_bytes(index)
                    .context("a plot's frame has no pixels")?;
                let mut pixels = pixels.to_vec();
                self.darken_pixels(&mut pixels);
                let buffer = image::RgbaImage::from_raw(
                    u32::try_from(size.width.0)?,
                    u32::try_from(size.height.0)?,
                    pixels,
                )
                .context("a plot's pixels don't match its size")?;
                Ok(image::Frame::new(buffer))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Arc::new(RenderImage::new(frames)))
    }

    /// Darkens BGRA pixels in place, leaving their alpha.
    pub(crate) fn darken_pixels(&self, pixels: &mut [u8]) {
        // A table per channel, from flipped value to its color, in BGRA order.
        let table = |background: f32, foreground: f32| -> [u8; 256] {
            std::array::from_fn(|value| {
                let fraction = value as f32 / 255.;
                let channel = background + fraction * (foreground - background);
                (channel.clamp(0., 1.) * 255.).round() as u8
            })
        };
        let tables = [
            table(self.background.b, self.foreground.b),
            table(self.background.g, self.foreground.g),
            table(self.background.r, self.foreground.r),
        ];
        for pixel in pixels.chunks_exact_mut(4) {
            let color = &mut pixel[..3];
            let brightest = color.iter().copied().max().unwrap_or(0);
            let darkest = color.iter().copied().min().unwrap_or(0);
            // Adding this to each channel flips HSL lightness exactly while
            // keeping hue and saturation, and stays within 0 to 255.
            let shift = 255 - i32::from(brightest) - i32::from(darkest);
            for (channel, table) in color.iter_mut().zip(&tables) {
                let flipped = (i32::from(*channel) + shift).clamp(0, 255) as usize;
                *channel = table.get(flipped).copied().unwrap_or(*channel);
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_darken_pixels() {
        let darkening = Darkening {
            background: rgb(0x202020),
            foreground: rgb(0xe0e0e0),
        };
        // BGRA: white, black, pure red, and mid gray half transparent.
        let mut pixels = vec![
            255, 255, 255, 255, //
            0, 0, 0, 255, //
            0, 0, 255, 255, //
            128, 128, 128, 100,
        ];
        darkening.darken_pixels(&mut pixels);
        assert_eq!(
            pixels[0..4],
            [0x20, 0x20, 0x20, 255],
            "white becomes the background"
        );
        assert_eq!(
            pixels[4..8],
            [0xe0, 0xe0, 0xe0, 255],
            "black becomes the text color"
        );
        assert_eq!(pixels[8..12], [0x20, 0x20, 0xe0, 255], "red stays red");
        assert_eq!(pixels[15], 100, "alpha is kept");
        assert!(
            pixels[12] == pixels[13] && pixels[13] == pixels[14],
            "gray stays gray: {pixels:?}"
        );
        // A light color turns dark and keeps its hue: light blue to dark blue.
        let mut light_blue = vec![255, 200, 150, 255];
        darkening.darken_pixels(&mut light_blue);
        let [blue, green, red, _] = light_blue[..] else {
            panic!("one pixel");
        };
        assert!(blue > green && green > red, "{light_blue:?}");
        assert!(blue < 200, "{light_blue:?}");
    }
}
