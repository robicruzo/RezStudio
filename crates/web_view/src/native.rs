//! The web view on macOS: a WKWebView, made and driven through `wry`.

use crate::files::{FILE_SCHEME, respond};
use crate::{NativeEvent, NativeView, PageStyle, is_allowed_url, page_style_script};
use anyhow::{Context as _, Result};
use futures::channel::mpsc;
use gpui::{Bounds, Pixels, Rgba, Window};
use objc2::{MainThreadMarker, MainThreadOnly as _};
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSImage, NSView};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGMutablePath;
use objc2_foundation::{NSDictionary, NSError, NSString};
use objc2_quartz_core::{CAShapeLayer, kCAFillRuleEvenOdd};
use objc2_web_kit::{WKUserScript, WKUserScriptInjectionTime};
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::http::Response;
use wry::{NewWindowResponse, PageLoadEvent, RGBA, Rect, WebViewBuilder, WebViewExtMacOS as _};

struct WryView {
    view: wry::WebView,
    events: mpsc::UnboundedSender<NativeEvent>,
}

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

fn rgba(color: Rgba) -> RGBA {
    let channel = |value: f32| (value.clamp(0., 1.) * 255.).round() as u8;
    (
        channel(color.r),
        channel(color.g),
        channel(color.b),
        channel(color.a),
    )
}

pub(crate) fn create(
    window: &Window,
    bounds: Bounds<Pixels>,
    events: mpsc::UnboundedSender<NativeEvent>,
    file_root: Arc<Mutex<Option<PathBuf>>>,
    style: Option<PageStyle>,
) -> Result<Box<dyn NativeView>> {
    let navigation_events = events.clone();
    let new_window_events = events.clone();
    let title_events = events.clone();
    let click_events = events.clone();
    let page_load_events = events.clone();
    let mut builder = WebViewBuilder::new()
        // GPUI doesn't see clicks on the page; the page says when it's clicked.
        .with_initialization_script(
            r#"document.addEventListener("mousedown", () => window.ipc.postMessage("clicked"), true);"#,
        )
        .with_ipc_handler(move |request| {
            if request.body() == "clicked" {
                click_events.unbounded_send(NativeEvent::Clicked).ok();
            }
        });
    if let Some(style) = style {
        builder = builder
            .with_initialization_script(page_style_script(&style.css)?)
            .with_background_color(rgba(style.background));
    }
    let view = builder
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
                page_load_events
                    .unbounded_send(NativeEvent::Loaded(url))
                    .ok();
            }
        })
        .build_as_child(window)
        .context("couldn't make a WKWebView")?;
    Ok(Box::new(WryView { view, events }))
}

fn png_bytes(image: &NSImage) -> Result<Vec<u8>> {
    let tiff = image
        .TIFFRepresentation()
        .context("the snapshot has no data")?;
    let bitmap = NSBitmapImageRep::imageRepWithData(&tiff).context("unreadable snapshot")?;
    let png = unsafe {
        bitmap.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }
    .context("couldn't encode the snapshot")?;
    Ok(png.to_vec())
}

impl NativeView for WryView {
    fn set_bounds(&self, bounds: Bounds<Pixels>) -> Result<()> {
        Ok(self.view.set_bounds(rect(bounds))?)
    }

    fn set_visible(&self, visible: bool) -> Result<()> {
        if !visible {
            self.release_keyboard()?;
        }
        Ok(self.view.set_visible(visible)?)
    }

    fn snapshot(&self) -> Result<()> {
        let events = RefCell::new(Some(self.events.clone()));
        let handler = block2::RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
            let Some(events) = events.borrow_mut().take() else {
                return;
            };
            // Safety: WebKit passes an image or an error, each valid for the call.
            let result = match (unsafe { image.as_ref() }, unsafe { error.as_ref() }) {
                (Some(image), _) => png_bytes(image),
                (None, Some(error)) => Err(anyhow::anyhow!(
                    "WebKit couldn't take a snapshot: {}",
                    error.localizedDescription()
                )),
                (None, None) => Err(anyhow::anyhow!("WebKit took no snapshot")),
            };
            match result {
                Ok(png) => {
                    events.unbounded_send(NativeEvent::Snapshot(png)).ok();
                }
                Err(error) => log::error!("{error:#}"),
            }
        });
        unsafe {
            self.view
                .webview()
                .takeSnapshotWithConfiguration_completionHandler(None, &handler)
        };
        Ok(())
    }

    fn set_cut_outs(&self, cut_outs: &[Bounds<Pixels>]) -> Result<()> {
        let webview = self.view.webview();
        webview.setWantsLayer(true);
        let layer = webview.layer().context("the web view has no layer")?;
        if cut_outs.is_empty() {
            unsafe { layer.setMask(None) };
            return Ok(());
        }
        let frame = layer.bounds();
        // In a layer whose geometry isn't flipped, y counts up from the bottom.
        let from_top = layer.isGeometryFlipped();
        // Even-odd filling shows the page everywhere but inside the cut-outs.
        let path = CGMutablePath::new();
        unsafe { CGMutablePath::add_rect(Some(&path), std::ptr::null(), frame) };
        for cut_out in cut_outs {
            let height = f64::from(f32::from(cut_out.size.height));
            let top = f64::from(f32::from(cut_out.origin.y));
            let y = if from_top {
                top
            } else {
                frame.size.height - top - height
            };
            let rect = CGRect::new(
                CGPoint::new(f64::from(f32::from(cut_out.origin.x)), y),
                CGSize::new(f64::from(f32::from(cut_out.size.width)), height),
            );
            unsafe { CGMutablePath::add_rect(Some(&path), std::ptr::null(), rect) };
        }
        let mask = CAShapeLayer::new();
        mask.setFrame(frame);
        mask.setPath(Some(&path));
        mask.setFillRule(unsafe { kCAFillRuleEvenOdd });
        unsafe { layer.setMask(Some(&mask)) };
        Ok(())
    }

    /// A clicked page keeps the window's keyboard input until something else
    /// in AppKit takes it, and GPUI's own focus changes don't, so hand it
    /// back to GPUI's view.
    fn release_keyboard(&self) -> Result<()> {
        let webview = self.view.webview();
        let Some(window) = webview.window() else {
            return Ok(());
        };
        let holds_keyboard = window.firstResponder().is_some_and(|responder| {
            responder
                .downcast_ref::<NSView>()
                .is_some_and(|view| view.isDescendantOf(&webview))
        });
        if holds_keyboard {
            let gpui_view = unsafe { webview.superview() };
            window.makeFirstResponder(gpui_view.as_deref().map(|view| &**view));
        }
        Ok(())
    }

    fn load_url(&self, url: &str) -> Result<()> {
        Ok(self.view.load_url(url)?)
    }

    fn evaluate_script(&self, script: &str) -> Result<()> {
        Ok(self.view.evaluate_script(script)?)
    }

    fn reload(&self) -> Result<()> {
        Ok(self.view.reload()?)
    }

    fn set_page_style(&self, style: &PageStyle) -> Result<()> {
        let script = page_style_script(&style.css)?;
        // wry only takes page scripts as the view is made, so this adds one to
        // WebKit directly. Each change adds another; they run in order, so the
        // newest style wins. Removing the old ones would remove wry's own.
        let main_thread = MainThreadMarker::new().context("not on the main thread")?;
        let user_script = unsafe {
            WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
                WKUserScript::alloc(main_thread),
                &NSString::from_str(&script),
                WKUserScriptInjectionTime::AtDocumentStart,
                true,
            )
        };
        unsafe { self.view.manager().addUserScript(&user_script) };
        self.view.evaluate_script(&script)?;
        Ok(self.view.set_background_color(rgba(style.background))?)
    }
}
