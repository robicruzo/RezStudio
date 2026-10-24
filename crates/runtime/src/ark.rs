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
        .args(["--session-mode", "console"])
        .current_dir(launch.working_directory)
        .env_clear()
        .envs(launch.env)
        .env("R_HOME", launch.r_home)
        // Ark logs at `info` by default, which floods the log.
        .env("RUST_LOG", "warn");

    let mut process =
        util::process::Child::spawn(command, Stdio::null(), Stdio::piped(), Stdio::piped())
            .with_context(|| format!("starting Ark at {}", launch.ark.display()))?;
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
/// that started it and stops Ark once that process is gone. Ark's exit status
/// is passed through, and the connection file is removed when Ark exits.
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

    #[test]
    fn test_supervisor_stops_program_when_parent_dies() {
        let temp = tempfile::tempdir().unwrap();
        let connection_file = temp.path().join("kernel.json");
        std::fs::write(&connection_file, "{}").unwrap();
        let pid_file = temp.path().join("fake-ark.pid");
        let program = temp.path().join("fake-ark");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\necho $$ > '{}'\nexec sleep 60\n",
                pid_file.display()
            ),
        )
        .unwrap();
        std::process::Command::new("chmod")
            .arg("+x")
            .arg(&program)
            .status()
            .unwrap();

        // A short-lived parent stands in for RezStudio: it starts the
        // supervisor in the background and exits right away.
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
            assert!(port > 0);

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
