//! A plot in its own window, like RStudio's Zoom: R redraws it to fill the
//! window whenever the window settles at a new size.

use crate::CloseZoomWindow;
use crate::fitting::{
    self, PlotImage, Viewport, ViewportChange, decode_plot, describe_render_error,
};
use anyhow::Result;
use gpui::{
    App, Bounds, Entity, FocusHandle, Focusable, Pixels, Size, Subscription, Task, TitlebarOptions,
    Window, WindowBounds, WindowOptions, px, size,
};
use runtime::comms::{PlotEvent, PlotRenderSettings, RenderParams};
use runtime::{RSession, SessionEvent};
use ui::prelude::*;
use util::ResultExt as _;

pub(crate) fn open(
    session: Entity<RSession>,
    comm_id: String,
    image: Option<PlotImage>,
    live: bool,
    cx: &mut App,
) -> Result<()> {
    let bounds = Bounds::centered(None, size(px(800.), px(600.)), cx);
    cx.open_window(
        WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some("Plot".into()),
                ..Default::default()
            }),
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        |window, cx| cx.new(|cx| PlotZoomWindow::new(session, comm_id, image, live, window, cx)),
    )?;
    Ok(())
}

pub struct PlotZoomWindow {
    session: Entity<RSession>,
    comm_id: String,
    /// Starts as the Plots pane's image, shown scaled until R redraws it here.
    image: Option<PlotImage>,
    /// The settings `image` was drawn with by this window, if it was.
    rendered: Option<PlotRenderSettings>,
    live: bool,
    viewport: Viewport,
    pending: Option<PlotRenderSettings>,
    error: Option<String>,
    focus_handle: FocusHandle,
    _render_task: Option<Task<()>>,
    _resize_task: Option<Task<()>>,
    _subscription: Subscription,
}

impl PlotZoomWindow {
    fn new(
        session: Entity<RSession>,
        comm_id: String,
        image: Option<PlotImage>,
        live: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.subscribe_in(&session, window, |this, _, event, _, cx| {
            this.handle_session_event(event, cx)
        });
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        if live {
            let metadata = session.update(cx, |session, cx| session.plot_metadata(&comm_id, cx));
            cx.spawn_in(window, async move |_, cx| {
                if let Some(metadata) = metadata.await.log_err() {
                    cx.update(|window, _| {
                        window.set_window_title(&format!("{} — Plot", metadata.name))
                    })
                    .log_err();
                }
            })
            .detach();
        }
        Self {
            session,
            comm_id,
            image,
            rendered: None,
            live,
            viewport: Viewport::default(),
            pending: None,
            error: None,
            focus_handle,
            _render_task: None,
            _resize_task: None,
            _subscription: subscription,
        }
    }

    fn handle_session_event(&mut self, event: &SessionEvent, cx: &mut Context<Self>) {
        match event {
            SessionEvent::PlotEvent {
                comm_id,
                event: PlotEvent::Update { .. },
            } if *comm_id == self.comm_id => {
                // R's pre-render is drawn at the pane's size, not this window's.
                self.rendered = None;
                self.pending = None;
                self.ensure_render(cx);
            }
            SessionEvent::PlotClosed { comm_id } if *comm_id == self.comm_id => {
                self.live = false;
                cx.notify();
            }
            _ => {}
        }
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
        self.ensure_render(cx);
    }

    fn ensure_render(&mut self, cx: &mut Context<Self>) {
        let Some(settings) = self.viewport.render_settings() else {
            return;
        };
        if !self.live || self.rendered == Some(settings) || self.pending == Some(settings) {
            return;
        }
        self.pending = Some(settings);
        let render = self.session.update(cx, |session, cx| {
            session.render_plot(
                &self.comm_id,
                RenderParams {
                    size: Some(settings.size),
                    pixel_ratio: settings.pixel_ratio,
                    format: settings.format,
                },
                cx,
            )
        });
        // Replacing this task drops an older render, so only the latest is shown.
        self._render_task = Some(cx.spawn(async move |this, cx| {
            let image = match render.await {
                Ok(result) => {
                    let decode = cx.update(|cx| decode_plot(result, cx));
                    decode.await
                }
                Err(error) => Err(anyhow::anyhow!(describe_render_error(&error))),
            };
            this.update(cx, |this, cx| {
                this.pending = None;
                match image {
                    Ok(image) => {
                        // The first image is the pane's, so only this window's own are freed.
                        if let Some(replaced) = this.image.replace(image)
                            && this.rendered.is_some()
                        {
                            cx.drop_image(replaced.render, None);
                        }
                        this.rendered = Some(settings);
                        this.error = None;
                    }
                    Err(error) => this.error = Some(format!("{error:#}")),
                }
                this.ensure_render(cx);
                cx.notify();
            })
            .log_err();
        }));
    }

    fn close(&mut self, _: &CloseZoomWindow, window: &mut Window, _: &mut Context<Self>) {
        window.remove_window();
    }
}

#[cfg(test)]
impl PlotZoomWindow {
    pub(crate) fn has_own_image_for_test(&self) -> bool {
        self.image.is_some() && self.rendered.is_some()
    }
}

impl Focusable for PlotZoomWindow {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PlotZoomWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let image = self.image.as_ref().map(|image| image.render.clone());
        let error = match (&self.error, self.live) {
            (Some(error), _) => Some(error.clone()),
            (None, false) => {
                Some("R has restarted, so this plot can't be redrawn at a new size.".to_string())
            }
            (None, true) => None,
        };
        let placeholder = image.is_none().then_some("Drawing…");
        div()
            .key_context("PlotZoomWindow")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::close))
            .relative()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .text_color(cx.theme().colors().text)
            .child(fitting::measure(entity, Self::set_viewport))
            .children(image.map(fitting::plot_image))
            .children(error.map(|error| fitting::error_overlay(error.into(), cx)))
            .children(placeholder.map(fitting::placeholder))
    }
}
