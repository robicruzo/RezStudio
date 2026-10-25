//! Local HTML files, served to the web view under a `rezfile://` scheme from
//! the shown file's folder only. WebKit won't let a page loaded straight from
//! `file://` read the files beside it, which htmlwidgets need.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};

pub const FILE_SCHEME: &str = "rezfile";

/// Everything but what's safe in a URL path, so `/` stays a separator.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'/')
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// The URL the web view loads a local file at.
pub fn file_url(path: &Path) -> String {
    format!(
        "{FILE_SCHEME}://localhost{}",
        utf8_percent_encode(&path.to_string_lossy(), PATH_SEGMENT)
    )
}

/// The file a `rezfile://` request asks for, if it's inside `root`.
pub fn requested_file(url_path: &str, root: &Path) -> Option<PathBuf> {
    let path = PathBuf::from(percent_decode_str(url_path).decode_utf8().ok()?.as_ref());
    let escapes = path
        .components()
        .any(|component| matches!(component, Component::ParentDir));
    (path.is_absolute() && !escapes && path.starts_with(root)).then_some(path)
}

pub fn content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

/// What a `rezfile://` request gets: the file, or 403/404.
pub fn respond(url_path: &str, root: Option<&Path>) -> (u16, &'static str, Cow<'static, [u8]>) {
    let Some(path) = root.and_then(|root| requested_file(url_path, root)) else {
        return (403, "text/plain", Cow::Borrowed(b"Forbidden"));
    };
    match std::fs::read(&path) {
        Ok(bytes) => (200, content_type(&path), Cow::Owned(bytes)),
        Err(_) => (404, "text/plain", Cow::Borrowed(b"Not found")),
    }
}

/// Whether the view should go to `url` itself: its own files and pages on
/// this machine (help, local apps). Anything else opens in the browser.
pub fn is_allowed_url(url: &str) -> bool {
    if url.starts_with(&format!("{FILE_SCHEME}://")) || url == "about:blank" {
        return true;
    }
    let Some(rest) = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
    else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = match authority.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "0.0.0.0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_urls_round_trip_inside_the_root() {
        let path = Path::new("/tmp/Rtmp X/viewhtml1/index.html");
        let url = file_url(path);
        assert_eq!(url, "rezfile://localhost/tmp/Rtmp%20X/viewhtml1/index.html");
        let url_path = url.strip_prefix("rezfile://localhost").unwrap();
        let root = Path::new("/tmp/Rtmp X/viewhtml1");
        assert_eq!(requested_file(url_path, root).as_deref(), Some(path));
        // Its dependencies beside it are served too...
        assert!(requested_file("/tmp/Rtmp%20X/viewhtml1/lib/plotly.js", root).is_some());
        // ...but nothing outside its folder, however the path is spelled.
        assert!(requested_file("/tmp/Rtmp%20X/secret.txt", root).is_none());
        assert!(requested_file("/tmp/Rtmp%20X/viewhtml1/../secret.txt", root).is_none());
        assert!(requested_file("/etc/passwd", root).is_none());
        assert_eq!(respond("/etc/passwd", Some(root)).0, 403);
        assert_eq!(respond("/tmp/x", None).0, 403);
    }

    #[test]
    fn test_content_types() {
        assert_eq!(
            content_type(Path::new("a/index.HTML")),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            content_type(Path::new("lib/plotly.min.js")),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            content_type(Path::new("x.unknown")),
            "application/octet-stream"
        );
    }

    #[test]
    fn test_allowed_urls() {
        for url in [
            "rezfile://localhost/tmp/a.html",
            "http://127.0.0.1:23456/library/base/html/mean.html",
            "http://localhost:3838/",
            "about:blank",
        ] {
            assert!(is_allowed_url(url), "{url}");
        }
        for url in [
            "https://cran.r-project.org/",
            "file:///etc/passwd",
            "javascript:alert(1)",
        ] {
            assert!(!is_allowed_url(url), "{url}");
        }
    }
}
