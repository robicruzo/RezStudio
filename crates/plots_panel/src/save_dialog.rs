//! The Save Plot dialog: a format and a size, before choosing where to save.

use crate::{PlotExport, PlotsPanel};
use editor::Editor;
use gpui::{DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, WeakEntity, Window};
use runtime::comms::{PlotRenderFormat, PlotSize};
use ui::{Button, ButtonStyle, prelude::*};
use util::ResultExt as _;
use workspace::ModalView;

/// Larger than this, R and the image formats struggle.
const MAX_SIZE: i64 = 20_000;
const FORMATS: [(PlotRenderFormat, &str); 3] = [
    (PlotRenderFormat::Png, "PNG"),
    (PlotRenderFormat::Svg, "SVG"),
    (PlotRenderFormat::Pdf, "PDF"),
];

pub struct SavePlotDialog {
    panel: WeakEntity<PlotsPanel>,
    comm_id: String,
    format: PlotRenderFormat,
    width: Entity<Editor>,
    height: Entity<Editor>,
    error: Option<SharedString>,
}

impl SavePlotDialog {
    pub(crate) fn new(
        panel: WeakEntity<PlotsPanel>,
        comm_id: String,
        export: PlotExport,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let number_editor = |value: i64, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_text(value.to_string(), window, cx);
                editor.select_all(&Default::default(), window, cx);
                editor
            })
        };
        let width = number_editor(export.size.width, window, cx);
        let height = number_editor(export.size.height, window, cx);
        Self {
            panel,
            comm_id,
            format: export.format,
            width,
            height,
            error: None,
        }
    }

    fn parse_size(&self, cx: &App) -> Result<PlotSize, SharedString> {
        let parse = |editor: &Entity<Editor>, name: &str| {
            let text = editor.read(cx).text(cx);
            match text.trim().parse::<i64>() {
                Ok(value) if (1..=MAX_SIZE).contains(&value) => Ok(value),
                _ => Err(SharedString::from(format!(
                    "The {name} must be a whole number of pixels from 1 to {MAX_SIZE}."
                ))),
            }
        };
        Ok(PlotSize {
            width: parse(&self.width, "width")?,
            height: parse(&self.height, "height")?,
        })
    }

    pub(crate) fn confirm(&mut self, _: &menu::Confirm, _: &mut Window, cx: &mut Context<Self>) {
        let size = match self.parse_size(cx) {
            Ok(size) => size,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        let export = PlotExport {
            format: self.format,
            size,
        };
        let comm_id = self.comm_id.clone();
        self.panel
            .update(cx, |panel, cx| panel.save_plot(comm_id, export, cx))
            .log_err();
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn render_field(
        &self,
        label: &'static str,
        editor: &Entity<Editor>,
        cx: &App,
    ) -> impl IntoElement {
        h_flex()
            .gap_2()
            .child(
                div()
                    .w(rems(4.))
                    .child(Label::new(label).color(Color::Muted)),
            )
            .child(
                div()
                    .w(rems(6.))
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .child(editor.clone()),
            )
            .child(Label::new("px").color(Color::Muted))
    }
}

#[cfg(test)]
impl SavePlotDialog {
    pub(crate) fn set_for_test(
        &mut self,
        format: PlotRenderFormat,
        width: &str,
        height: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.format = format;
        self.width
            .update(cx, |editor, cx| editor.set_text(width, window, cx));
        self.height
            .update(cx, |editor, cx| editor.set_text(height, window, cx));
    }

    pub(crate) fn error_for_test(&self) -> Option<SharedString> {
        self.error.clone()
    }

    pub(crate) fn values_for_test(&self, cx: &App) -> (PlotRenderFormat, String, String) {
        (
            self.format,
            self.width.read(cx).text(cx),
            self.height.read(cx).text(cx),
        )
    }
}

impl EventEmitter<DismissEvent> for SavePlotDialog {}

impl ModalView for SavePlotDialog {}

impl Focusable for SavePlotDialog {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.width.focus_handle(cx)
    }
}

impl Render for SavePlotDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let formats = FORMATS.iter().map(|(format, label)| {
            let format = *format;
            Button::new(SharedString::from(*label), *label)
                .style(ButtonStyle::Subtle)
                .toggle_state(self.format == format)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.format = format;
                    cx.notify();
                }))
        });
        v_flex()
            .key_context("SavePlotDialog")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .w(rems(22.))
            .elevation_2(cx)
            .p_3()
            .gap_3()
            .child(Label::new("Save Plot"))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .w(rems(4.))
                            .child(Label::new("Format").color(Color::Muted)),
                    )
                    .children(formats),
            )
            .child(self.render_field("Width", &self.width, cx))
            .child(self.render_field("Height", &self.height, cx))
            .children(
                self.error
                    .clone()
                    .map(|error| Label::new(error).size(LabelSize::Small).color(Color::Error)),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("cancel", "Cancel").on_click(
                        cx.listener(|this, _, window, cx| this.cancel(&menu::Cancel, window, cx)),
                    ))
                    .child(
                        Button::new("save", "Save…")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&menu::Confirm, window, cx)
                            })),
                    ),
            )
    }
}
