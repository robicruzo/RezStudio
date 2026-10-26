use anyhow::{Context as _, Result};
use collections::HashMap;
use futures::{AsyncBufReadExt as _, StreamExt as _, io::BufReader};
use jupyter_protocol::connection_info::{ConnectionInfo, Transport};
use runtimelib::{
    ClientControlConnection, ClientIoPubConnection, ClientShellConnection, ClientStdinConnection,
};
use smol::net::TcpListener;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use util::ResultExt as _;

/// Overrides which Ark binary is launched, for trying other Ark builds.
pub const ARK_PATH_ENV_VAR: &str = "REZSTUDIO_ARK";

/// Finds the Ark binary: an explicit override, then the copy bundled in
/// RezStudio.app, then `ark` on the project's `PATH`.
pub fn locate_ark(bundled_ark: Option<PathBuf>, env: &HashMap<String, String>) -> Result<PathBuf> {
    if let Ok(path) = std::env::var(ARK_PATH_ENV_VAR) {
        let path = PathBuf::from(path);
        anyhow::ensure!(
            path.is_file(),
            "{ARK_PATH_ENV_VAR} points to {}, which is not a file",
            path.display()
        );
        return Ok(path);
    }
    if let Some(bundled_ark) = bundled_ark.filter(|path| path.is_file()) {
        return Ok(bundled_ark);
    }
    if let Some(path_ark) = env.get("PATH").and_then(|path_var| {
        std::env::split_paths(path_var)
            .map(|dir| dir.join("ark"))
            .find(|candidate| candidate.is_file())
    }) {
        return Ok(path_ark);
    }
    // Development builds aren't bundled with Ark, so fall back to Positron's copy.
    if cfg!(debug_assertions) {
        let positron_ark = PathBuf::from(
            "/Applications/Positron.app/Contents/Resources/app/extensions/positron-r/resources/ark/ark",
        );
        if positron_ark.is_file() {
            return Ok(positron_ark);
        }
    }
    anyhow::bail!(
        "Could not find Ark, the R kernel. Set {ARK_PATH_ENV_VAR} to an Ark binary or put `ark` on your PATH."
    )
}

pub struct ArkLaunch<'a> {
    pub ark: &'a Path,
    pub r_home: &'a Path,
    pub working_directory: &'a Path,
    pub env: HashMap<String, String>,
    /// Where Ark writes its own log. Without one Ark logs to stderr, which
    /// it captures into the console along with R's output.
    pub log_file: Option<PathBuf>,
}

/// A running Ark process. Dropping it kills the process.
pub struct ArkProcess {
    process: util::process::Child,
    connection_path: PathBuf,
}

impl ArkProcess {
    pub fn kill(&mut self) {
        self.process.kill().log_err();
    }

    /// The id of the shell supervising Ark, whose child Ark is.
    pub fn id(&self) -> u32 {
        self.process.id()
    }

    pub fn status(
        &mut self,
    ) -> impl Future<Output = std::io::Result<std::process::ExitStatus>> + use<> {
        self.process.status()
    }
}

impl Drop for ArkProcess {
    fn drop(&mut self) {
        self.kill();
        std::fs::remove_file(&self.connection_path).ok();
    }
}

pub struct ArkConnections {
    pub iopub: ClientIoPubConnection,
    pub shell: ClientShellConnection,
    pub control: ClientControlConnection,
    pub stdin: ClientStdinConnection,
}

/// Starts Ark in console mode and connects to its Jupyter sockets.
pub async fn launch_ark(launch: ArkLaunch<'_>) -> Result<(ArkProcess, ArkConnections)> {
    let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let ports = free_ports(ip).await?;
    let connection_info = ConnectionInfo {
        transport: Transport::TCP,
        ip: ip.to_string(),
        stdin_port: ports[0],
        control_port: ports[1],
        hb_port: ports[2],
        shell_port: ports[3],
        iopub_port: ports[4],
        signature_scheme: "hmac-sha256".to_string(),
        key: uuid::Uuid::new_v4().to_string(),
        kernel_name: Some("rezstudio-ark".to_string()),
    };

    let runtime_dir = runtimelib::dirs::runtime_dir();
    smol::fs::create_dir_all(&runtime_dir)
        .await
        .with_context(|| format!("creating Jupyter runtime directory {runtime_dir:?}"))?;
    let connection_path =
        runtime_dir.join(format!("kernel-rezstudio-{}.json", uuid::Uuid::new_v4()));
    smol::fs::write(&connection_path, serde_json::to_vec(&connection_info)?)
        .await
        .with_context(|| format!("writing connection file {connection_path:?}"))?;

    let mut command = supervised_command(launch.ark, &connection_path);
    command
        .arg("--connection_file")
        .arg(&connection_path)
        .args(["--session-mode", "console"]);
    if let Some(log_file) = &launch.log_file {
        command.arg("--log").arg(log_file);
    }
    command
        .current_dir(launch.working_directory)
        .env_clear()
        .envs(launch.env)
        .env("R_HOME", launch.r_home)
        // Ark logs at `info` by default, which floods the log.
        .env("RUST_LOG", "warn");

    // The spawn error's context repeats the whole supervised command line,
    // which is noise in the console; the root cause says what went wrong.
    let mut process =
        util::process::Child::spawn(command, Stdio::null(), Stdio::piped(), Stdio::piped())
            .map_err(|error| {
                log::error!("failed to spawn Ark: {error:?}");
                anyhow::anyhow!(
                    "Couldn't run Ark at {}: {}",
                    launch.ark.display(),
                    error.root_cause()
                )
            })?;
    forward_output_to_log(&mut process);
    let process = ArkProcess {
        process,
        connection_path,
    };

    let session_id = uuid::Uuid::new_v4().to_string();
    let peer_identity = runtimelib::peer_identity_for_session(&session_id)?;
    let connections = ArkConnections {
        iopub: runtimelib::create_client_iopub_connection(&connection_info, "", &session_id)
            .await?,
        shell: runtimelib::create_client_shell_connection_with_identity(
            &connection_info,
            &session_id,
            peer_identity.clone(),
        )
        .await?,
        control: runtimelib::create_client_control_connection(&connection_info, &session_id)
            .await?,
        stdin: runtimelib::create_client_stdin_connection_with_identity(
            &connection_info,
            &session_id,
            peer_identity,
        )
        .await?,
    };
    Ok((process, connections))
}

/// Ark doesn't notice when its frontend dies, so a crashed or force-quit
/// RezStudio would leave R running. This shell wrapper polls for the process
/// that started it and stops Ark once that process is gone. Ark can ignore
/// SIGTERM, so after a few seconds it's killed outright. Ark's exit status is
/// passed through, and the connection file is removed when Ark exits.
const SUPERVISOR_SCRIPT: &str = r#"
connection_file="$1"
ark="$2"
shift 2
parent_pid=$PPID
"$ark" "$@" &
ark_pid=$!
(
  while kill -0 "$parent_pid" 2>/dev/null; do sleep 1; done
  # Ark can take its whole process group down when stopped, so clean up first.
  rm -f "$connection_file"
  kill -TERM "$ark_pid" 2>/dev/null
  waited=0
  while kill -0 "$ark_pid" 2>/dev/null && [ "$waited" -lt 5 ]; do
    sleep 1
    waited=$((waited + 1))
  done
  kill -KILL "$ark_pid" 2>/dev/null
) &
watcher_pid=$!
wait "$ark_pid"
status=$?
kill "$watcher_pid" 2>/dev/null
rm -f "$connection_file"
exit $status
"#;

/// Returns a command running `program` under [`SUPERVISOR_SCRIPT`]; arguments
/// added to it are passed to `program`.
fn supervised_command(program: &Path, connection_path: &Path) -> std::process::Command {
    let mut command = util::command::new_std_command("/bin/sh");
    command
        .arg("-c")
        .arg(SUPERVISOR_SCRIPT)
        .arg("rezstudio-ark-supervisor")
        .arg(connection_path)
        .arg(program);
    command
}

const KEPT_ARK_LOGS: usize = 5;

/// Returns a new log file path for an Ark session in `logs_dir`, removing all
/// but the most recent earlier ones.
pub fn next_log_file(logs_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(logs_dir)
        .with_context(|| format!("creating {}", logs_dir.display()))?;
    let mut existing: Vec<PathBuf> = std::fs::read_dir(logs_dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("ark-") && name.ends_with(".log"))
        })
        .collect();
    // Names start with a sortable timestamp, so the oldest sort first.
    existing.sort();
    let excess = (existing.len() + 1).saturating_sub(KEPT_ARK_LOGS);
    for old in existing.iter().take(excess) {
        std::fs::remove_file(old).log_err();
    }
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    Ok(logs_dir.join(format!("ark-{timestamp}-{}.log", std::process::id())))
}

fn forward_output_to_log(process: &mut util::process::Child) {
    let stdout = process.stdout.take();
    let stderr = process.stderr.take();
    smol::spawn(async move {
        let stdout = stdout.map(|stdout| BufReader::new(stdout).lines().boxed());
        let stderr = stderr.map(|stderr| BufReader::new(stderr).lines().boxed());
        let mut lines = futures::stream::select(
            stdout.unwrap_or_else(|| futures::stream::empty().boxed()),
            stderr.unwrap_or_else(|| futures::stream::empty().boxed()),
        );
        while let Some(Ok(line)) = lines.next().await {
            log::warn!("ark: {line}");
        }
    })
    .detach();
}

/// Picks five free ports by binding and releasing them. Another process can
/// take a port before Ark binds it, a race inherent to Jupyter connection files.
async fn free_ports(ip: IpAddr) -> Result<[u16; 5]> {
    let mut listeners = Vec::with_capacity(5);
    for _ in 0..5 {
        listeners.push(TcpListener::bind(SocketAddr::new(ip, 0)).await?);
    }
    let mut ports = [0; 5];
    for (port, listener) in ports.iter_mut().zip(&listeners) {
        *port = listener.local_addr()?.port();
    }
    Ok(ports)
}

// These tests run real processes on plain threads, outside GPUI's test
// scheduler, so blocking process calls and smol timers are what they need.
#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod supervisor_tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    fn is_running(pid: &str) -> bool {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[test]
    fn test_next_log_file_keeps_recent_logs() {
        let temp = tempfile::tempdir().unwrap();
        for timestamp in 1..=7 {
            std::fs::write(temp.path().join(format!("ark-{timestamp:04}-1.log")), "").unwrap();
        }
        std::fs::write(temp.path().join("unrelated.txt"), "").unwrap();

        let next = next_log_file(temp.path()).unwrap();
        assert!(
            next.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("ark-")
        );
        let mut remaining: Vec<String> = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        remaining.sort();
        assert_eq!(
            remaining,
            [
                "ark-0004-1.log",
                "ark-0005-1.log",
                "ark-0006-1.log",
                "ark-0007-1.log",
                "unrelated.txt"
            ]
        );
    }

    #[test]
    fn test_supervisor_passes_exit_status_through() {
        let temp = tempfile::tempdir().unwrap();
        let connection_file = temp.path().join("kernel.json");
        std::fs::write(&connection_file, "{}").unwrap();
        let program = temp.path().join("fake-ark");
        std::fs::write(&program, "#!/bin/sh\necho \"args: $*\"\nexit 3\n").unwrap();
        std::process::Command::new("chmod")
            .arg("+x")
            .arg(&program)
            .status()
            .unwrap();

        let output = supervised_command(&program, &connection_file)
            .args(["--connection_file", "x"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "args: --connection_file x\n"
        );
        assert!(!connection_file.exists());
    }

    /// Runs a fake Ark doing `body` under the supervisor, from a parent that
    /// exits right away as a crashed RezStudio would, and asserts the fake is
    /// stopped and the connection file removed.
    fn assert_stopped_when_parent_dies(body: &str) {
        let temp = tempfile::tempdir().unwrap();
        let connection_file = temp.path().join("kernel.json");
        std::fs::write(&connection_file, "{}").unwrap();
        let pid_file = temp.path().join("fake-ark.pid");
        let program = temp.path().join("fake-ark");
        std::fs::write(
            &program,
            format!("#!/bin/sh\necho $$ > '{}'\n{body}\n", pid_file.display()),
        )
        .unwrap();
        std::process::Command::new("chmod")
            .arg("+x")
            .arg(&program)
            .status()
            .unwrap();

        let supervisor = supervised_command(&program, &connection_file);
        let mut parent = std::process::Command::new("/bin/sh");
        parent
            .arg("-c")
            .arg(r#""$@" >/dev/null 2>&1 & sleep 1"#)
            .arg("parent");
        parent.arg(supervisor.get_program());
        parent.args(supervisor.get_args());
        assert!(parent.status().unwrap().success());

        assert!(wait_for(|| pid_file.exists()), "the program never started");
        let pid = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .to_string();
        assert!(
            wait_for(|| !is_running(&pid)),
            "the program kept running after its parent exited"
        );
        assert!(wait_for(|| !connection_file.exists()));
    }

    #[test]
    fn test_supervisor_stops_program_when_parent_dies() {
        assert_stopped_when_parent_dies("exec sleep 60");
    }

    #[test]
    fn test_supervisor_kills_program_that_ignores_sigterm() {
        // As Ark did, leaving R running after RezStudio quit.
        assert_stopped_when_parent_dies("trap '' TERM\nwhile true; do sleep 1; done");
    }
}

/// Drives a real Ark with a real R through the requests RezStudio relies on.
/// Needs R (`R` on PATH) and Ark (`REZSTUDIO_ARK`, `ark` on PATH, or Positron's
/// copy in debug builds): `cargo test -p runtime -- --ignored`.
#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod integration_tests {
    use super::*;
    use crate::comms::{LspCommMessage, UiFrontendEvent, parse_method};
    use async_dispatcher::{Dispatcher, Runnable};
    use futures::FutureExt as _;
    use jupyter_protocol::{
        CommId, CommOpen, ExecuteRequest, InputReply, InterruptRequest, IsCompleteReplyStatus,
        IsCompleteRequest, JupyterMessage, JupyterMessageContent, KernelInfoRequest, ReplyStatus,
        ShutdownRequest,
    };
    use std::time::{Duration, Instant};

    struct SmolDispatcher;

    impl Dispatcher for SmolDispatcher {
        fn dispatch(&self, runnable: Runnable) {
            smol::spawn(async move {
                runnable.run();
            })
            .detach();
        }

        fn dispatch_after(&self, duration: Duration, runnable: Runnable) {
            smol::spawn(async move {
                smol::Timer::after(duration).await;
                runnable.run();
            })
            .detach();
        }
    }

    struct Kernel {
        _process: ArkProcess,
        connections: ArkConnections,
    }

    impl Kernel {
        async fn send_shell(
            &mut self,
            content: impl Into<JupyterMessageContent>,
        ) -> JupyterMessage {
            let message = JupyterMessage::new(content, None);
            self.connections.shell.send(message.clone()).await.unwrap();
            message
        }

        /// Reads every channel until `done` returns true for a message, and
        /// returns everything read along the way.
        async fn read_until(
            &mut self,
            mut done: impl FnMut(&str, &JupyterMessage) -> bool,
        ) -> Vec<(&'static str, JupyterMessage)> {
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut seen = Vec::new();
            loop {
                assert!(
                    Instant::now() < deadline,
                    "timed out; saw {:#?}",
                    summarize(&seen)
                );
                let (channel, message) = futures::select! {
                    message = self.connections.iopub.read().fuse() => ("iopub", message),
                    message = self.connections.shell.read().fuse() => ("shell", message),
                    message = self.connections.stdin.read().fuse() => ("stdin", message),
                    message = self.connections.control.read().fuse() => ("control", message),
                    _ = futures::FutureExt::fuse(smol::Timer::after(Duration::from_millis(500))) => continue,
                };
                let message = message.unwrap();
                let finished = done(channel, &message);
                seen.push((channel, message));
                if finished {
                    return seen;
                }
            }
        }

        /// Sends a comm RPC and waits for Ark's reply, returning its result.
        async fn comm_rpc(
            &mut self,
            comm_id: &CommId,
            method: &str,
            params: serde_json::Value,
        ) -> serde_json::Value {
            self.try_comm_rpc(comm_id, method, params)
                .await
                .unwrap_or_else(|error| panic!("{method} failed: {error}"))
        }

        /// Like [`Self::comm_rpc`], returning Ark's error rather than panicking.
        async fn try_comm_rpc(
            &mut self,
            comm_id: &CommId,
            method: &str,
            params: serde_json::Value,
        ) -> anyhow::Result<serde_json::Value> {
            let mut message = JupyterMessage::new(
                jupyter_protocol::CommMsg {
                    comm_id: comm_id.clone(),
                    data: serde_json::Map::new(),
                },
                None,
            );
            let data = crate::comms::rpc_request_data(&message.header.msg_id, method, params);
            if let (JupyterMessageContent::CommMsg(comm), serde_json::Value::Object(data)) =
                (&mut message.content, data)
            {
                comm.data = data;
            }
            self.connections.shell.send(message.clone()).await.unwrap();
            let messages = self
                .read_until(|_, reply| {
                    reply.header.msg_type == "comm_msg" && is_child_of(reply, &message)
                })
                .await;
            let reply = data_object(&messages.last().unwrap().1).unwrap();
            crate::comms::rpc_reply_result(reply)
        }

        async fn execute(
            &mut self,
            code: &str,
        ) -> (JupyterMessage, Vec<(&'static str, JupyterMessage)>) {
            let request = self
                .send_shell(ExecuteRequest {
                    code: code.to_string(),
                    allow_stdin: true,
                    ..ExecuteRequest::default()
                })
                .await;
            let messages = self.read_until_finished(&request).await;
            (request, messages)
        }

        /// Outputs travel on iopub and the reply on shell, so neither alone
        /// means an execution is over: wait for both the reply and iopub idle.
        async fn read_until_finished(
            &mut self,
            request: &JupyterMessage,
        ) -> Vec<(&'static str, JupyterMessage)> {
            let mut replied = false;
            let mut idle = false;
            self.read_until(|_, message| {
                replied |= is_reply_to(message, request, "execute_reply");
                idle |= is_idle_for(message, request);
                replied && idle
            })
            .await
        }
    }

    fn is_reply_to(message: &JupyterMessage, request: &JupyterMessage, msg_type: &str) -> bool {
        message.header.msg_type == msg_type
            && message.parent_header.as_ref().map(|header| &header.msg_id)
                == Some(&request.header.msg_id)
    }

    /// A page's body, fetched over plain HTTP from a server on this machine.
    fn http_get(url: &str) -> String {
        use std::io::{Read as _, Write as _};
        use std::net::TcpStream;

        let rest = url.strip_prefix("http://").unwrap();
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let mut stream = TcpStream::connect(authority).unwrap();
        stream
            .write_all(format!("GET /{path} HTTP/1.0\r\nHost: {authority}\r\n\r\n").as_bytes())
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(
            response.starts_with("HTTP/1.") && response.contains(" 200 "),
            "{response:.200}"
        );
        response
    }

    /// A PNG's width and height, from its header.
    fn png_size(bytes: &[u8]) -> (u32, u32) {
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"), "not a PNG");
        let read = |start: usize| {
            u32::from_be_bytes(bytes.get(start..start + 4).unwrap().try_into().unwrap())
        };
        (read(16), read(20))
    }

    fn is_child_of(message: &JupyterMessage, request: &JupyterMessage) -> bool {
        message.parent_header.as_ref().map(|header| &header.msg_id) == Some(&request.header.msg_id)
    }

    fn is_idle_for(message: &JupyterMessage, request: &JupyterMessage) -> bool {
        matches!(
            &message.content,
            JupyterMessageContent::Status(status) if status.execution_state == jupyter_protocol::ExecutionState::Idle
        ) && message.parent_header.as_ref().map(|header| &header.msg_id)
            == Some(&request.header.msg_id)
    }

    fn summarize(messages: &[(&'static str, JupyterMessage)]) -> Vec<String> {
        messages
            .iter()
            .map(|(channel, message)| {
                format!(
                    "{channel}: {} {}",
                    message.header.msg_type,
                    serde_json::to_string(&message.content).unwrap_or_default()
                )
            })
            .collect()
    }

    fn stdout_text(messages: &[(&'static str, JupyterMessage)]) -> String {
        messages
            .iter()
            .filter_map(|(_, message)| match &message.content {
                JupyterMessageContent::StreamContent(stream) => Some(stream.text.clone()),
                JupyterMessageContent::ExecuteResult(result) => serde_json::to_value(&result.data)
                    .ok()
                    .map(|data| data.to_string()),
                _ => None,
            })
            .collect()
    }

    fn execute_reply_status(messages: &[(&'static str, JupyterMessage)]) -> ReplyStatus {
        messages
            .iter()
            .find_map(|(_, message)| match &message.content {
                JupyterMessageContent::ExecuteReply(reply) => Some(reply.status.clone()),
                _ => None,
            })
            .expect("an execute_reply")
    }

    fn r_home() -> PathBuf {
        let output = std::process::Command::new("R")
            .arg("RHOME")
            .output()
            .expect("R must be on PATH");
        PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
    }

    fn data_object(message: &JupyterMessage) -> Option<serde_json::Value> {
        match &message.content {
            JupyterMessageContent::CommMsg(comm) => {
                Some(serde_json::Value::Object(comm.data.clone()))
            }
            _ => None,
        }
    }

    /// Talks to Ark's language server the way a plain LSP client does and
    /// returns the completion labels offered at `line`/`character` in `text`.
    fn lsp_completion_labels(port: u16, text: &str, line: u32, character: u32) -> Vec<String> {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpStream;

        let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        let mut send = |message: serde_json::Value| {
            let body = message.to_string();
            write!(writer, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
            writer.flush().unwrap();
        };
        let receive = |reader: &mut BufReader<TcpStream>| -> serde_json::Value {
            let mut length = 0;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                let header = header.trim();
                if header.is_empty() {
                    break;
                }
                if let Some(value) = header.strip_prefix("Content-Length:") {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            serde_json::from_slice(&body).unwrap()
        };
        let uri = "file:///tmp/rezstudio-lsp-test.R";

        send(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "processId": null, "rootUri": null, "capabilities": {} }
        }));
        let mut pending_id = 1;
        let completion = loop {
            let message = receive(&mut reader);
            // Answer requests from the server so it never waits on us.
            if let (Some(id), Some(method)) = (message.get("id"), message.get("method")) {
                let result = if method == "workspace/configuration" {
                    let count = message["params"]["items"].as_array().map_or(0, Vec::len);
                    serde_json::Value::Array(vec![serde_json::Value::Null; count])
                } else {
                    serde_json::Value::Null
                };
                send(serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }));
                continue;
            }
            if message.get("id") != Some(&serde_json::json!(pending_id)) {
                continue;
            }
            if pending_id == 1 {
                send(
                    serde_json::json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
                );
                send(serde_json::json!({
                    "jsonrpc": "2.0", "method": "textDocument/didOpen",
                    "params": { "textDocument": { "uri": uri, "languageId": "r", "version": 1, "text": text } }
                }));
                pending_id = 2;
                send(serde_json::json!({
                    "jsonrpc": "2.0", "id": 2, "method": "textDocument/completion",
                    "params": { "textDocument": { "uri": uri }, "position": { "line": line, "character": character } }
                }));
            } else {
                break message["result"].clone();
            }
        };
        let items = completion
            .get("items")
            .cloned()
            .unwrap_or(completion)
            .as_array()
            .cloned()
            .unwrap_or_default();
        items
            .iter()
            .filter_map(|item| item["label"].as_str().map(str::to_string))
            .collect()
    }

    #[test]
    #[ignore = "needs R and Ark"]
    fn test_real_ark_session() {
        async_dispatcher::set_dispatcher(SmolDispatcher);
        smol::block_on(async {
            let env: HashMap<String, String> = std::env::vars().collect();
            let ark = locate_ark(None, &env).unwrap();
            let working_directory = tempfile::tempdir().unwrap();
            let (process, connections) = launch_ark(ArkLaunch {
                ark: &ark,
                r_home: &r_home(),
                working_directory: working_directory.path(),
                env,
                log_file: Some(working_directory.path().join("ark.log")),
            })
            .await
            .unwrap();
            let mut kernel = Kernel {
                _process: process,
                connections,
            };

            let kernel_info = kernel.send_shell(KernelInfoRequest {}).await;
            kernel
                .read_until(|_, message| is_reply_to(message, &kernel_info, "kernel_info_reply"))
                .await;

            // Opening the UI comm makes Ark report its prompt.
            let ui_comm = CommId(uuid::Uuid::new_v4().to_string());
            kernel
                .send_shell(CommOpen {
                    comm_id: ui_comm.clone(),
                    target_name: crate::comms::UI_COMM_TARGET.to_string(),
                    data: serde_json::from_value(serde_json::json!({ "console_width": 80 }))
                        .unwrap(),
                    target_module: None,
                })
                .await;
            kernel
                .read_until(|_, message| {
                    data_object(message)
                        .and_then(|data| parse_method::<UiFrontendEvent>(data).ok())
                        .is_some_and(|event| matches!(event, UiFrontendEvent::PromptState(_)))
                })
                .await;

            let lsp_comm = CommId(uuid::Uuid::new_v4().to_string());
            kernel
                .send_shell(CommOpen {
                    comm_id: lsp_comm.clone(),
                    target_name: crate::comms::LSP_COMM_TARGET.to_string(),
                    data: serde_json::from_value(serde_json::json!({ "ip_address": "127.0.0.1" }))
                        .unwrap(),
                    target_module: None,
                })
                .await;
            let messages = kernel
                .read_until(|_, message| {
                    data_object(message)
                        .and_then(|data| serde_json::from_value::<LspCommMessage>(data).ok())
                        .is_some()
                })
                .await;
            let LspCommMessage::ServerStarted { port } = data_object(&messages.last().unwrap().1)
                .and_then(|data| serde_json::from_value(data).ok())
                .unwrap();
            // The console's document never exists on disk, like this one; Ark
            // still completes from packages and from variables in the session.
            let (_, messages) = kernel.execute("my_value <- 42").await;
            assert_eq!(execute_reply_status(&messages), ReplyStatus::Ok);
            let completions =
                std::thread::spawn(move || lsp_completion_labels(port, "x <- me + my_v", 0, 14))
                    .join()
                    .unwrap();
            assert!(
                completions.iter().any(|label| label == "my_value"),
                "{completions:?}"
            );
            assert!(
                completions.iter().any(|label| label == "mean"),
                "{completions:?}"
            );

            // A language server connects once per LSP comm, so a second one,
            // as Zed starts when the toolchain changes, gets a new comm and
            // port, as Positron does when it restarts its language server.
            kernel
                .send_shell(jupyter_protocol::CommClose {
                    comm_id: lsp_comm.clone(),
                    data: serde_json::Map::new(),
                })
                .await;
            let second_comm = CommId(uuid::Uuid::new_v4().to_string());
            kernel
                .send_shell(CommOpen {
                    comm_id: second_comm.clone(),
                    target_name: crate::comms::LSP_COMM_TARGET.to_string(),
                    data: serde_json::from_value(serde_json::json!({ "ip_address": "127.0.0.1" }))
                        .unwrap(),
                    target_module: None,
                })
                .await;
            let messages = kernel
                .read_until(|_, message| {
                    matches!(&message.content, JupyterMessageContent::CommMsg(comm) if comm.comm_id == second_comm)
                        && data_object(message)
                            .and_then(|data| serde_json::from_value::<LspCommMessage>(data).ok())
                            .is_some()
                })
                .await;
            let LspCommMessage::ServerStarted { port: second_port } =
                data_object(&messages.last().unwrap().1)
                    .and_then(|data| serde_json::from_value(data).ok())
                    .unwrap();
            let completions = std::thread::spawn(move || {
                lsp_completion_labels(second_port, "x <- me + my_v", 0, 14)
            })
            .join()
            .unwrap();
            assert!(
                completions.iter().any(|label| label == "my_value"),
                "the second language server: {completions:?}"
            );

            let (_, messages) = kernel.execute("x <- 1 + 1\ncat(x, '\\n')").await;
            assert_eq!(execute_reply_status(&messages), ReplyStatus::Ok);
            assert!(
                stdout_text(&messages).contains('2'),
                "{:#?}",
                summarize(&messages)
            );

            let (_, messages) = kernel.execute("stop('boom')").await;
            assert_eq!(execute_reply_status(&messages), ReplyStatus::Error);
            assert!(
                messages.iter().any(|(_, message)| matches!(
                    &message.content,
                    JupyterMessageContent::ErrorOutput(error) if error.evalue.contains("boom")
                )),
                "{:#?}",
                summarize(&messages)
            );

            let request = kernel
                .send_shell(IsCompleteRequest {
                    code: "mean(".into(),
                })
                .await;
            let messages = kernel
                .read_until(|_, message| is_reply_to(message, &request, "is_complete_reply"))
                .await;
            assert!(matches!(
                &messages.last().unwrap().1.content,
                JupyterMessageContent::IsCompleteReply(reply) if reply.status == IsCompleteReplyStatus::Incomplete
            ));

            let request = kernel
                .send_shell(ExecuteRequest {
                    code: "name <- readline('Name: '); cat('Hello', name, '\\n')".into(),
                    allow_stdin: true,
                    ..ExecuteRequest::default()
                })
                .await;
            let messages = kernel
                .read_until(|channel, message| {
                    channel == "stdin" && message.header.msg_type == "input_request"
                })
                .await;
            let input_request = messages.last().unwrap().1.clone();
            kernel
                .connections
                .stdin
                .send(JupyterMessage::new(
                    InputReply {
                        value: "Ada".into(),
                        status: ReplyStatus::Ok,
                        error: None,
                    },
                    Some(&input_request),
                ))
                .await
                .unwrap();
            let messages = kernel.read_until_finished(&request).await;
            assert!(
                stdout_text(&messages).contains("Hello Ada"),
                "{:#?}",
                summarize(&messages)
            );

            let request = kernel
                .send_shell(ExecuteRequest {
                    code: "Sys.sleep(30)".into(),
                    ..ExecuteRequest::default()
                })
                .await;
            smol::Timer::after(Duration::from_secs(1)).await;
            let started = Instant::now();
            kernel
                .connections
                .control
                .send(JupyterMessage::new(InterruptRequest {}, None))
                .await
                .unwrap();
            kernel.read_until_finished(&request).await;
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "interrupt took too long"
            );

            // R still works after the interrupt.
            let (_, messages) = kernel.execute("cat(40 + 2)").await;
            assert!(stdout_text(&messages).contains("42"));

            // The variables comm lists the session's variables, then reports changes.
            let parse_variables = |message: &JupyterMessage| {
                data_object(message)
                    .and_then(|data| parse_method::<crate::comms::VariablesEvent>(data).ok())
            };
            let variables_comm = CommId(uuid::Uuid::new_v4().to_string());
            kernel
                .send_shell(CommOpen {
                    comm_id: variables_comm.clone(),
                    target_name: crate::comms::VARIABLES_COMM_TARGET.to_string(),
                    data: serde_json::Map::new(),
                    target_module: None,
                })
                .await;
            let messages = kernel
                .read_until(|_, message| {
                    matches!(
                        parse_variables(message),
                        Some(crate::comms::VariablesEvent::Refresh(_))
                    )
                })
                .await;
            let Some(crate::comms::VariablesEvent::Refresh(refresh)) =
                parse_variables(&messages.last().unwrap().1)
            else {
                panic!("expected a refresh");
            };
            assert!(
                refresh
                    .variables
                    .iter()
                    .any(|variable| variable.display_name == "my_value")
            );

            let request = kernel
                .send_shell(ExecuteRequest {
                    code: "nested <- list(a = 1, b = letters)".into(),
                    ..ExecuteRequest::default()
                })
                .await;
            let messages = kernel.read_until_finished(&request).await;
            let update = messages
                .iter()
                .find_map(|(_, message)| match parse_variables(message) {
                    Some(crate::comms::VariablesEvent::Update(update)) => Some(update),
                    _ => None,
                });
            let update = match update {
                Some(update) => update,
                None => {
                    let messages = kernel
                        .read_until(|_, message| {
                            matches!(
                                parse_variables(message),
                                Some(crate::comms::VariablesEvent::Update(_))
                            )
                        })
                        .await;
                    let Some(crate::comms::VariablesEvent::Update(update)) =
                        parse_variables(&messages.last().unwrap().1)
                    else {
                        panic!("expected an update");
                    };
                    update
                }
            };
            let nested = update
                .assigned
                .iter()
                .find(|variable| variable.display_name == "nested")
                .expect("nested in the update")
                .clone();
            assert!(nested.has_children);
            let children: crate::comms::InspectedVariable = serde_json::from_value(
                kernel
                    .comm_rpc(
                        &variables_comm,
                        "inspect",
                        serde_json::json!({ "path": [nested.access_key] }),
                    )
                    .await,
            )
            .unwrap();
            let names: Vec<_> = children
                .children
                .iter()
                .map(|child| child.display_name.as_str())
                .collect();
            assert_eq!(names, ["a", "b"]);

            // View() of a 10M-row data frame opens a data explorer that serves
            // pages without the frame leaving R.
            let (_, messages) = kernel
                .execute(
                    "big <- data.frame(id = seq_len(1e7), x = rnorm(1e7), g = sample(c(letters, NA), 1e7, TRUE)); View(big)",
                )
                .await;
            let explorer = messages
                .iter()
                .find_map(|(_, message)| match &message.content {
                    JupyterMessageContent::CommOpen(open)
                        if open.target_name == crate::comms::DATA_EXPLORER_COMM_TARGET =>
                    {
                        Some(open.clone())
                    }
                    _ => None,
                })
                .expect("View() opens a data explorer");
            let open_data: crate::comms::DataExplorerOpenData =
                serde_json::from_value(serde_json::Value::Object(explorer.data.clone())).unwrap();
            assert_eq!(open_data.title, "big");

            let started = Instant::now();
            let state: crate::comms::BackendState = serde_json::from_value(
                kernel
                    .comm_rpc(&explorer.comm_id, "get_state", serde_json::Value::Null)
                    .await,
            )
            .unwrap();
            assert_eq!(state.table_shape.num_rows, 10_000_000);
            assert_eq!(state.table_shape.num_columns, 3);
            let schema: crate::comms::TableSchema = serde_json::from_value(
                kernel
                    .comm_rpc(
                        &explorer.comm_id,
                        "get_schema",
                        serde_json::json!({ "column_indices": [0, 1, 2] }),
                    )
                    .await,
            )
            .unwrap();
            let types: Vec<_> = schema
                .columns
                .iter()
                .map(|column| column.type_display)
                .collect();
            assert_eq!(
                types,
                [
                    crate::comms::ColumnDisplayType::Integer,
                    crate::comms::ColumnDisplayType::Floating,
                    crate::comms::ColumnDisplayType::String
                ]
            );
            let metadata_time = started.elapsed();
            let page = |first: i64| {
                serde_json::to_value(crate::comms::GetDataValuesParams {
                    columns: (0..3)
                        .map(|column_index| crate::comms::ColumnSelection {
                            column_index,
                            spec: crate::comms::ArraySelection::SelectRange(
                                crate::comms::DataSelectionRange {
                                    first_index: first,
                                    last_index: first + 99,
                                },
                            ),
                        })
                        .collect(),
                    format_options: crate::comms::FormatOptions::default(),
                })
                .unwrap()
            };
            let page_started = Instant::now();
            let data: crate::comms::TableData = serde_json::from_value(
                kernel
                    .comm_rpc(&explorer.comm_id, "get_data_values", page(5_000_000))
                    .await,
            )
            .unwrap();
            let page_time = page_started.elapsed();
            assert_eq!(data.columns.len(), 3);
            assert_eq!(data.columns[0].len(), 100);
            assert_eq!(data.columns[0][0].display().0, "5000001");
            eprintln!(
                "data explorer: state and schema in {metadata_time:?}, a 100-row page from the middle in {page_time:?}"
            );
            assert!(
                page_time < Duration::from_secs(1),
                "a page took {page_time:?}"
            );

            // Sorting happens in R; the first row afterwards has the largest id.
            kernel
                .comm_rpc(
                    &explorer.comm_id,
                    "set_sort_columns",
                    serde_json::json!({ "sort_keys": [{ "column_index": 0, "ascending": false }] }),
                )
                .await;
            let data: crate::comms::TableData = serde_json::from_value(
                kernel
                    .comm_rpc(&explorer.comm_id, "get_data_values", page(0))
                    .await,
            )
            .unwrap();
            assert_eq!(data.columns[0][0].display().0, "10000000");

            // Missing counts arrive later in a `return_column_profiles` event.
            let parse_explorer = |message: &JupyterMessage| {
                data_object(message)
                    .and_then(|data| parse_method::<crate::comms::DataExplorerEvent>(data).ok())
            };
            kernel
                .comm_rpc(
                    &explorer.comm_id,
                    "get_column_profiles",
                    serde_json::to_value(crate::comms::GetColumnProfilesParams {
                        callback_id: "nulls".to_string(),
                        profiles: (0..3)
                            .map(|column_index| crate::comms::ColumnProfileRequest {
                                column_index,
                                profiles: vec![crate::comms::ColumnProfileSpec {
                                    profile_type: crate::comms::ColumnProfileType::NullCount,
                                    params: None,
                                }],
                            })
                            .collect(),
                        format_options: crate::comms::FormatOptions::default(),
                    })
                    .unwrap(),
                )
                .await;
            let messages = kernel
                .read_until(|_, message| {
                    matches!(
                        parse_explorer(message),
                        Some(crate::comms::DataExplorerEvent::ReturnColumnProfiles(_))
                    )
                })
                .await;
            let Some(crate::comms::DataExplorerEvent::ReturnColumnProfiles(profiles)) =
                parse_explorer(&messages.last().unwrap().1)
            else {
                panic!("expected column profiles");
            };
            assert_eq!(profiles.callback_id, "nulls");
            assert_eq!(profiles.profiles[0].null_count, Some(0));
            assert!(
                profiles.profiles[2]
                    .null_count
                    .is_some_and(|count| count > 0)
            );

            // Each new plot gets a comm with a PNG drawn at the size the
            // frontend last asked for, and can be redrawn at any size.
            let settings = crate::comms::PlotRenderSettings {
                size: crate::comms::PlotSize {
                    width: 500,
                    height: 300,
                },
                pixel_ratio: 2.0,
                format: crate::comms::PlotRenderFormat::Png,
            };
            kernel
                .send_shell(jupyter_protocol::CommMsg {
                    comm_id: ui_comm.clone(),
                    data: serde_json::from_value(
                        serde_json::to_value(
                            crate::comms::UiBackendEvent::DidChangePlotsRenderSettings { settings },
                        )
                        .unwrap(),
                    )
                    .unwrap(),
                })
                .await;
            let (plot_request, messages) = kernel.execute("plot(1:10)").await;
            // The session tags a plot with the execution still running when its
            // comm opens, so the comm must open before that execution goes idle
            // (both come on iopub, in order).
            let opened = messages.iter().position(|(_, message)| {
                matches!(&message.content, JupyterMessageContent::CommOpen(open) if open.target_name == crate::comms::PLOT_COMM_TARGET)
            });
            let idle = messages
                .iter()
                .position(|(_, message)| is_idle_for(message, &plot_request));
            assert!(
                opened.zip(idle).is_some_and(|(opened, idle)| opened < idle),
                "{:#?}",
                summarize(&messages)
            );
            let plot_open = |messages: &[(&'static str, JupyterMessage)]| {
                messages
                    .iter()
                    .find_map(|(_, message)| match &message.content {
                        JupyterMessageContent::CommOpen(open)
                            if open.target_name == crate::comms::PLOT_COMM_TARGET =>
                        {
                            Some(open.clone())
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| panic!("no plot comm in {:#?}", summarize(messages)))
            };
            let plot = plot_open(&messages);
            let open_data: crate::comms::PlotOpenData =
                serde_json::from_value(serde_json::Value::Object(plot.data.clone())).unwrap();
            let pre_render = open_data.pre_render.expect("a new plot comes drawn");
            assert_eq!(pre_render.mime_type, "image/png");
            assert_eq!(png_size(&pre_render.decode().unwrap()), (1000, 600));

            let render = |width: i64, height: i64, format: &str| {
                serde_json::json!({
                    "size": { "width": width, "height": height },
                    "pixel_ratio": 2.0,
                    "format": format,
                })
            };
            let render_started = Instant::now();
            let png: crate::comms::PlotResult = serde_json::from_value(
                kernel
                    .comm_rpc(&plot.comm_id, "render", render(800, 600, "png"))
                    .await,
            )
            .unwrap();
            let render_time = render_started.elapsed();
            assert_eq!(png_size(&png.decode().unwrap()), (1600, 1200));
            eprintln!("plots: an 800×600 render at 2x in {render_time:?}");
            let svg: crate::comms::PlotResult = serde_json::from_value(
                kernel
                    .comm_rpc(&plot.comm_id, "render", render(800, 600, "svg"))
                    .await,
            )
            .unwrap();
            assert_eq!(svg.mime_type, "image/svg+xml");
            let svg_text = String::from_utf8(svg.decode().unwrap()).unwrap();
            assert!(svg_text.contains("<svg"), "{svg_text:.200}");
            let pdf: crate::comms::PlotResult = serde_json::from_value(
                kernel
                    .comm_rpc(&plot.comm_id, "render", render(800, 600, "pdf"))
                    .await,
            )
            .unwrap();
            assert_eq!(pdf.mime_type, "application/pdf");
            assert!(pdf.decode().unwrap().starts_with(b"%PDF"));
            let metadata: crate::comms::PlotMetadata = serde_json::from_value(
                kernel
                    .comm_rpc(&plot.comm_id, "get_metadata", serde_json::Value::Null)
                    .await,
            )
            .unwrap();
            assert!(!metadata.name.is_empty(), "{metadata:?}");
            assert_eq!(metadata.code, "plot(1:10)");

            // Drawing on the same plot updates it rather than opening another.
            let (_, messages) = kernel.execute("lines(10:1)").await;
            let update = messages
                .iter()
                .find_map(|(_, message)| match &message.content {
                    JupyterMessageContent::CommMsg(comm) if comm.comm_id == plot.comm_id => {
                        parse_method::<crate::comms::PlotEvent>(serde_json::Value::Object(
                            comm.data.clone(),
                        ))
                        .ok()
                    }
                    _ => None,
                });
            let Some(crate::comms::PlotEvent::Update {
                pre_render: Some(updated),
            }) = update
            else {
                panic!("expected an update in {:#?}", summarize(&messages));
            };
            assert_eq!(png_size(&updated.decode().unwrap()), (1000, 600));

            // Closing a plot's comm frees it; the next plot gets a new comm.
            kernel
                .send_shell(jupyter_protocol::CommClose {
                    comm_id: plot.comm_id.clone(),
                    data: serde_json::Map::new(),
                })
                .await;
            let (_, messages) = kernel.execute("plot(2:3)").await;
            assert_ne!(plot_open(&messages).comm_id, plot.comm_id);

            // Help pages come over the help comm, as URLs on Ark's help proxy.
            let help_comm = CommId(uuid::Uuid::new_v4().to_string());
            kernel
                .send_shell(CommOpen {
                    comm_id: help_comm.clone(),
                    target_name: crate::comms::HELP_COMM_TARGET.to_string(),
                    data: serde_json::Map::new(),
                    target_module: None,
                })
                .await;
            let help_event = |messages: &[(&'static str, JupyterMessage)]| {
                messages
                    .iter()
                    .find_map(|(_, message)| match &message.content {
                        JupyterMessageContent::CommMsg(comm) if comm.comm_id == help_comm => {
                            parse_method::<crate::comms::HelpEvent>(serde_json::Value::Object(
                                comm.data.clone(),
                            ))
                            .ok()
                        }
                        _ => None,
                    })
            };
            let (_, messages) = kernel.execute("?mean").await;
            let Some(crate::comms::HelpEvent::ShowHelp(help)) = help_event(&messages) else {
                panic!("no help in {:#?}", summarize(&messages));
            };
            assert_eq!(help.kind, crate::comms::ShowHelpKind::Url);
            assert!(
                help.content.starts_with("http://127.0.0.1:"),
                "{}",
                help.content
            );
            let url = help.content.clone();
            let page = std::thread::spawn(move || http_get(&url)).join().unwrap();
            assert!(page.contains("Arithmetic Mean"), "{page:.300}");
            let found = kernel
                .comm_rpc(
                    &help_comm,
                    "show_help_topic",
                    serde_json::json!({ "topic": "lm" }),
                )
                .await;
            assert_eq!(found, serde_json::json!(true));

            // The Help pane's Home runs `help.start()` silently, through the UI
            // comm, and the index arrives as a help page like any other. The
            // event can come before or after the reply, so wait for both.
            let mut request = JupyterMessage::new(
                jupyter_protocol::CommMsg {
                    comm_id: ui_comm.clone(),
                    data: serde_json::Map::new(),
                },
                None,
            );
            if let (JupyterMessageContent::CommMsg(comm), serde_json::Value::Object(data)) = (
                &mut request.content,
                crate::comms::rpc_request_data(
                    &request.header.msg_id,
                    "evaluate_code",
                    serde_json::json!({ "code": "utils::help.start()" }),
                ),
            ) {
                comm.data = data;
            }
            kernel
                .connections
                .shell
                .send(request.clone())
                .await
                .unwrap();
            let mut replied = false;
            let mut index_url = None;
            kernel
                .read_until(|_, message| {
                    replied |=
                        message.header.msg_type == "comm_msg" && is_child_of(message, &request);
                    // The page `show_help_topic` found above may come first.
                    if let Some(crate::comms::HelpEvent::ShowHelp(help)) =
                        help_event(&[("iopub", message.clone())])
                        && help.content.ends_with("/doc/html/index.html")
                    {
                        index_url = Some(help.content);
                    }
                    replied && index_url.is_some()
                })
                .await;
            let index = std::thread::spawn(move || http_get(&index_url.unwrap_or_default()))
                .join()
                .unwrap();
            assert!(index.contains("Packages"), "{index:.300}");

            // R's viewer option (htmlwidgets use it) shows temporary HTML files,
            // and `browseURL()` on a local address is for the Viewer too.
            let ui_event = |messages: &[(&'static str, JupyterMessage)], method: &str| {
                messages
                    .iter()
                    .find_map(|(_, message)| match &message.content {
                        JupyterMessageContent::CommMsg(comm)
                            if comm.comm_id == ui_comm && comm.data["method"] == method =>
                        {
                            Some(comm.data["params"].clone())
                        }
                        _ => None,
                    })
            };
            let (_, messages) = kernel
                .execute("page <- file.path(tempdir(), 'widget.html'); writeLines('<p>hi</p>', page); getOption('viewer')(page); normalizePath(page)")
                .await;
            let shown = ui_event(&messages, "show_html_file")
                .unwrap_or_else(|| panic!("no show_html_file in {:#?}", summarize(&messages)));
            let shown_path = shown["path"].as_str().unwrap_or_default().to_string();
            assert!(shown_path.ends_with("widget.html"), "{shown}");
            assert!(std::path::Path::new(&shown_path).exists(), "{shown}");
            let (_, messages) = kernel.execute("browseURL('http://127.0.0.1:5678/')").await;
            let shown = ui_event(&messages, "show_url")
                .unwrap_or_else(|| panic!("no show_url in {:#?}", summarize(&messages)));
            assert_eq!(shown["url"], "http://127.0.0.1:5678/");

            // Ark's functions for a Packages pane, through the UI comm.
            let call_method = |method: &str, params: serde_json::Value| serde_json::json!({ "method": method, "params": params });
            let installed = |reply: serde_json::Value| {
                serde_json::from_value::<Vec<crate::comms::InstalledPackage>>(reply).unwrap()
            };
            let attached = |packages: &[crate::comms::InstalledPackage], name: &str| {
                packages
                    .iter()
                    .find(|package| package.name == name)
                    .unwrap_or_else(|| panic!("{name} isn't installed"))
                    .attached
            };
            let started = Instant::now();
            let packages = installed(
                kernel
                    .comm_rpc(
                        &ui_comm,
                        "call_method",
                        call_method("pkg_list", serde_json::json!([])),
                    )
                    .await,
            );
            eprintln!(
                "packages: {} installed, listed in {:?}",
                packages.len(),
                started.elapsed()
            );
            assert!(attached(&packages, "stats"));
            assert!(!attached(&packages, "tools"));
            let (_, messages) = kernel.execute("library(tools)").await;
            assert_eq!(execute_reply_status(&messages), ReplyStatus::Ok);
            let packages = installed(
                kernel
                    .comm_rpc(
                        &ui_comm,
                        "call_method",
                        call_method("pkg_list", serde_json::json!([])),
                    )
                    .await,
            );
            assert!(attached(&packages, "tools"));
            let detail: Option<crate::comms::PackageDetail> = serde_json::from_value(
                kernel
                    .comm_rpc(
                        &ui_comm,
                        "call_method",
                        call_method("pkg_detail", serde_json::json!(["stats"])),
                    )
                    .await,
            )
            .unwrap();
            let detail = detail.expect("stats is installed");
            assert!(
                detail
                    .title
                    .as_deref()
                    .is_some_and(|title| title.contains("Stats")),
                "{detail:?}"
            );
            let missing = kernel
                .comm_rpc(
                    &ui_comm,
                    "call_method",
                    call_method("pkg_detail", serde_json::json!(["notAPackageAnywhere"])),
                )
                .await;
            assert_eq!(missing, serde_json::Value::Null);
            let evaluated: crate::comms::EvalResult = serde_json::from_value(
                kernel
                    .comm_rpc(
                        &ui_comm,
                        "evaluate_code",
                        serde_json::json!({ "code": "nzchar(Sys.getenv('RENV_PROJECT'))" }),
                    )
                    .await,
            )
            .unwrap();
            assert_eq!(evaluated.result, serde_json::json!(false));
            let unknown = kernel
                .try_comm_rpc(
                    &ui_comm,
                    "call_method",
                    call_method("no_such_method", serde_json::json!([])),
                )
                .await;
            assert!(
                unknown
                    .as_ref()
                    .is_err_and(|error| error.to_string().contains("No such method")),
                "{unknown:?}"
            );

            kernel
                .connections
                .control
                .send(JupyterMessage::new(
                    ShutdownRequest { restart: false },
                    None,
                ))
                .await
                .unwrap();
            let exited = kernel._process.status();
            let exited = futures::future::select(
                Box::pin(exited),
                smol::Timer::after(Duration::from_secs(10)),
            )
            .await;
            assert!(
                matches!(exited, futures::future::Either::Left(_)),
                "Ark did not exit after shutdown"
            );
        });
    }
}
