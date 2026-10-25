//! Ark's language server runs inside the R process and listens on a TCP port,
//! while Zed only talks to language servers over stdio. The adapter below
//! starts `rezstudio --lsp-bridge <port>`, which copies between the two.

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use gpui::AsyncApp;
use language::{LspAdapter, LspAdapterDelegate, LspInstaller, Toolchain};
use lsp::{LanguageServerBinary, LanguageServerId, LanguageServerName};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use util::ResultExt as _;

use crate::{SessionRegistry, SessionState};

pub const ARK_LANGUAGE_SERVER_NAME: LanguageServerName = LanguageServerName::new_static("ark");

/// The hidden command-line flag that turns the RezStudio binary into the bridge.
pub const LSP_BRIDGE_FLAG: &str = "--lsp-bridge";

/// How long a language server start waits for R to report its port.
const PORT_WAIT_TIMEOUT: Duration = Duration::from_secs(60);
const PORT_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Runs the bridge: connects to Ark's language server on `port` and copies
/// stdin to it and its output to stdout until either side closes.
pub fn run_lsp_bridge(port: u16) -> Result<()> {
    let stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .with_context(|| format!("connecting to Ark's language server on port {port}"))?;
    bridge(stream, std::io::stdin(), std::io::stdout())
}

fn bridge(
    stream: TcpStream,
    mut input: impl Read + Send + 'static,
    mut output: impl Write,
) -> Result<()> {
    stream.set_nodelay(true).ok();
    let mut to_server = stream.try_clone()?;
    // When the editor closes stdin, close the socket's write half so Ark sees
    // the end of the stream; the copy below then ends when Ark hangs up.
    std::thread::spawn(move || {
        std::io::copy(&mut input, &mut to_server).ok();
        to_server.shutdown(std::net::Shutdown::Write).ok();
    });
    let mut from_server = stream;
    copy_flushing(&mut from_server, &mut output)?;
    Ok(())
}

/// Like `std::io::copy`, but flushes after every read so responses aren't
/// held in stdout's buffer.
fn copy_flushing(input: &mut impl Read, output: &mut impl Write) -> Result<()> {
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = match input.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        let Some(chunk) = buffer.get(..read) else {
            return Ok(());
        };
        output.write_all(chunk)?;
        output.flush()?;
    }
}

/// Offers Ark's language server for R files in projects with a running R session.
pub struct ArkLspAdapter;

impl ArkLspAdapter {
    /// Waits for the session of the project rooted at `root` to report its
    /// language server port. Returns `None` if there is no R to wait for.
    async fn wait_for_port(root: &Path, cx: &AsyncApp) -> Option<u16> {
        let started = std::time::Instant::now();
        loop {
            match cx.update(|cx| port_for_project(root, cx)) {
                PortStatus::Ready(port) => return Some(port),
                PortStatus::NoSession => return None,
                PortStatus::Pending if started.elapsed() > PORT_WAIT_TIMEOUT => {
                    log::warn!(
                        "R did not start its language server in time for {}",
                        root.display()
                    );
                    return None;
                }
                PortStatus::Pending => {}
            }
            cx.background_executor().timer(PORT_POLL_INTERVAL).await;
        }
    }
}

enum PortStatus {
    Ready(u16),
    Pending,
    NoSession,
}

fn port_for_project(root: &Path, cx: &gpui::App) -> PortStatus {
    let Some(registry) = SessionRegistry::global(cx) else {
        return PortStatus::NoSession;
    };
    let session = registry
        .read(cx)
        .sessions()
        .find(|session| session.read(cx).project_root() == root);
    let Some(session) = session else {
        // The session is created asynchronously after the project opens.
        return PortStatus::Pending;
    };
    let session = session.read(cx);
    match (session.language_server_port(), session.state()) {
        (Some(port), _) => PortStatus::Ready(port),
        (None, SessionState::Unavailable(_)) => PortStatus::NoSession,
        (None, _) => PortStatus::Pending,
    }
}

impl LspInstaller for ArkLspAdapter {
    type BinaryVersion = ();

    async fn check_if_user_installed(
        &self,
        delegate: &Arc<dyn LspAdapterDelegate>,
        _: Option<Toolchain>,
        cx: &AsyncApp,
    ) -> Option<LanguageServerBinary> {
        let port = Self::wait_for_port(delegate.worktree_root_path(), cx).await?;
        let bridge = std::env::current_exe().log_err()?;
        Some(LanguageServerBinary {
            path: bridge,
            arguments: vec![LSP_BRIDGE_FLAG.into(), port.to_string().into()],
            env: None,
        })
    }

    async fn fetch_latest_server_version(
        &self,
        _: &Arc<dyn LspAdapterDelegate>,
        _: bool,
        _: &mut AsyncApp,
    ) -> Result<()> {
        anyhow::bail!("Ark's language server runs inside R; it starts with the project's R session")
    }

    fn fetch_server_binary(
        &self,
        _: (),
        _: PathBuf,
        _: &Arc<dyn LspAdapterDelegate>,
    ) -> impl Send + Future<Output = Result<LanguageServerBinary>> + use<> {
        async { anyhow::bail!("Ark's language server cannot be downloaded") }
    }

    async fn cached_server_binary(
        &self,
        _: PathBuf,
        _: &dyn LspAdapterDelegate,
    ) -> Option<LanguageServerBinary> {
        None
    }
}

/// Documents with R chunks, which Ark's server also runs for.
pub const R_DOCUMENT_LANGUAGES: [&str; 2] = ["R Markdown", "Quarto"];

/// Their file types, whose diagnostics are dropped.
const R_DOCUMENT_EXTENSIONS: [&str; 5] = ["Rmd", "rmd", "Rmarkdown", "rmarkdown", "qmd"];

#[async_trait(?Send)]
impl LspAdapter for ArkLspAdapter {
    fn name(&self) -> LanguageServerName {
        ARK_LANGUAGE_SERVER_NAME
    }

    /// Ark reads an R Markdown or Quarto document as R, so its prose would be
    /// full of syntax errors.
    fn process_diagnostics(&self, params: &mut lsp::PublishDiagnosticsParams, _: LanguageServerId) {
        if is_r_document(&params.uri) {
            params.diagnostics.clear();
        }
    }
}

fn is_r_document(uri: &lsp::Uri) -> bool {
    let path = uri.path().to_string();
    R_DOCUMENT_EXTENSIONS
        .iter()
        .any(|extension| path.ends_with(&format!(".{extension}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;

    #[test]
    fn test_r_documents_lose_their_diagnostics() {
        let params = |path: &str| lsp::PublishDiagnosticsParams {
            uri: lsp::Uri::from_file_path(path).unwrap(),
            diagnostics: vec![lsp::Diagnostic::new_simple(
                Default::default(),
                "unexpected".into(),
            )],
            version: None,
        };
        let adapter = ArkLspAdapter;
        for (path, kept) in [
            ("/project/report.Rmd", 0),
            ("/project/paper.qmd", 0),
            ("/project/analysis.R", 1),
        ] {
            let mut params = params(path);
            adapter.process_diagnostics(&mut params, LanguageServerId(0));
            assert_eq!(params.diagnostics.len(), kept, "{path}");
        }
    }

    #[test]
    fn test_bridge_copies_both_ways_until_server_closes() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (received_tx, received_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0u8; 5];
            socket.read_exact(&mut request).unwrap();
            received_tx.send(request.to_vec()).unwrap();
            socket.write_all(b"pong").unwrap();
            // Dropping the socket ends the bridge.
        });

        let stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        let mut output = Vec::new();
        bridge(
            stream,
            std::io::Cursor::new(b"ping\n".to_vec()),
            &mut output,
        )
        .unwrap();
        server.join().unwrap();

        assert_eq!(received_rx.recv().unwrap(), b"ping\n");
        assert_eq!(output, b"pong");
    }
}
