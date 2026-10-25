//! The web view on macOS: a WKWebView, made and driven through `wry`.

use crate::files::{FILE_SCHEME, respond};
use crate::{NativeEvent, NativeView, is_allowed_url};
use anyhow::{Context as _, Result};
use futures::channel::mpsc;
use gpui::{Bounds, Pixels, Window};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::http::Response;
use wry::{NewWindowResponse, PageLoadEvent, Rect, WebViewBuilder};

struct WryView(wry::WebView);

fn rect(bounds: Bounds<Pixels>) -> Rect {
    Rect {
        position: LogicalPosition::new(
            f64::from(f32::from(bounds.origin.x)),
            f64::from(f32::from(bounds.origin.y)),
        )
        .into(),
        size: LogicalSize::new(
            f64::from(f32::from(bounds.size.width)),
            f64::from(f32::from(bounds.size.height)),
        )
        .into(),
    }
}

pub(crate) fn create(
    window: &Window,
    bounds: Bounds<Pixels>,
    events: mpsc::UnboundedSender<NativeEvent>,
    file_root: Arc<Mutex<Option<PathBuf>>>,
) -> Result<Box<dyn NativeView>> {
    let navigation_events = events.clone();
    let new_window_events = events.clone();
    let title_events = events.clone();
    let view = WebViewBuilder::new()
        .with_bounds(rect(bounds))
        .with_visible(false)
        .with_custom_protocol(FILE_SCHEME.to_string(), move |_, request| {
            let root = file_root.lock().ok().and_then(|root| root.clone());
            let (status, content_type, body) = respond(request.uri().path(), root.as_deref());
            Response::builder()
                .status(status)
                .header("Content-Type", content_type)
                .body(body)
                .unwrap_or_else(|_| Response::new(Vec::new().into()))
        })
        .with_navigation_handler(move |url| {
            if is_allowed_url(&url) {
                return true;
            }
            navigation_events
                .unbounded_send(NativeEvent::OpenExternally(url))
                .ok();
            false
        })
        .with_new_window_req_handler(move |url, _| {
            // Links that open a new window, as `target="_blank"` ones do.
            new_window_events
                .unbounded_send(NativeEvent::OpenExternally(url))
                .ok();
            NewWindowResponse::Deny
        })
        .with_document_title_changed_handler(move |title| {
            title_events
                .unbounded_send(NativeEvent::TitleChanged(title))
                .ok();
        })
        .with_on_page_load_handler(move |event, url| {
            if matches!(event, PageLoadEvent::Finished) {
                events.unbounded_send(NativeEvent::Loaded(url)).ok();
            }
        })
        .build_as_child(window)
        .context("couldn't make a WKWebView")?;
    Ok(Box::new(WryView(view)))
}

impl NativeView for WryView {
    fn set_bounds(&self, bounds: Bounds<Pixels>) -> Result<()> {
        Ok(self.0.set_bounds(rect(bounds))?)
    }

    fn set_visible(&self, visible: bool) -> Result<()> {
        Ok(self.0.set_visible(visible)?)
    }

    fn load_url(&self, url: &str) -> Result<()> {
        Ok(self.0.load_url(url)?)
    }

    fn evaluate_script(&self, script: &str) -> Result<()> {
        Ok(self.0.evaluate_script(script)?)
    }

    fn reload(&self) -> Result<()> {
        Ok(self.0.reload()?)
    }
}
