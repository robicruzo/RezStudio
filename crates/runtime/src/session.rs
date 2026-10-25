use crate::ark::{self, ArkLaunch, ArkProcess};
use crate::comms::{
    self, DATA_EXPLORER_COMM_TARGET, DataExplorerEvent, DataExplorerOpenData, LSP_COMM_TARGET,
    LspCommMessage, PLOT_COMM_TARGET, PlotEvent, PlotMetadata, PlotOpenData, PlotRenderSettings,
    PlotResult, PromptStateParams, RPC_REPLY_MSG_TYPE, RPC_REQUEST_MSG_TYPE, RenderParams,
    ServerStartData, UI_COMM_TARGET, UiBackendEvent, UiCommOpenData, UiFrontendEvent,
    UiFrontendRequest, VARIABLES_COMM_TARGET, VariablesEvent,
};
use anyhow::{Context as _, Result};
use collections::HashMap;
use futures::channel::{mpsc, oneshot};
use futures::{FutureExt as _, StreamExt as _};
use gpui::{
    AppContext as _, AsyncWindowContext, Context, EventEmitter, PromptLevel, Subscription, Task,
    WeakEntity, Window,
};
use jupyter_protocol::{
    CommClose, CommId, CommMsg, CommOpen, ExecuteRequest, InputReply, InterruptRequest,
    IsCompleteReplyStatus, IsCompleteRequest, JupyterMessage, JupyterMessageContent,
    KernelInfoRequest, ReplyStatus, ShutdownRequest, UnknownMessage,
};
use languages::r::RToolchainData;
use std::path::PathBuf;
use std::time::Duration;
use util::ResultExt as _;

/// Ark's default console width before the console pane reports its own.
const DEFAULT_CONSOLE_WIDTH: i32 = 80;
const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(3);
/// Redrawing a complex plot, such as a large ggplot2 scatter, can take a while.
const PLOT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq)]
pub enum SessionState {
    /// No usable R or Ark; the message says what is missing.
    Unavailable(String),
    /// The project folder isn't trusted yet. R runs the project's `.Rprofile`
    /// on startup, so it waits for trust like language servers do.
    Restricted,
    Starting,
    Idle,
    /// At least one execution is in flight.
    Busy,
    Restarting,
    Exited(String),
}

#[derive(Clone, Debug)]
pub enum SessionEvent {
    StateChanged,
    /// Output produced on behalf of an execution, identified by its request id
    /// when Ark attributes it to one.
    Output {
        request_id: Option<String>,
        content: JupyterMessageContent,
    },
    ExecutionFinished {
        request_id: String,
        succeeded: bool,
    },
    /// R is waiting for a line of input, as from `readline()` or `menu()`.
    InputRequested {
        prompt: String,
        password: bool,
    },
    PromptChanged,
    WorkingDirectoryChanged,
    ClearConsole,
    ShowMessage(String),
    OpenEditor {
        path: PathBuf,
        line: u32,
        column: u32,
    },
    LanguageServerStarted {
        port: u16,
    },
    /// A message on the variables comm, which the Environment pane mirrors.
    Variables(VariablesEvent),
    /// Ark opened a data explorer, as for `View(df)`.
    DataExplorerOpened {
        comm_id: String,
        title: String,
    },
    DataExplorerEvent {
        comm_id: String,
        event: DataExplorerEvent,
    },
    /// The explorer's comm is gone, because Ark closed it or R stopped.
    DataExplorerClosed {
        comm_id: String,
    },
    /// R drew a new plot. `pre_render` is drawn at the last size given to
    /// [`RSession::set_plot_render_settings`].
    PlotOpened {
        comm_id: String,
        pre_render: Option<PlotResult>,
    },
    PlotEvent {
        comm_id: String,
        event: PlotEvent,
    },
    /// The plot's comm is gone, because Ark closed it or R stopped, so it
    /// can't be drawn again.
    PlotClosed {
        comm_id: String,
    },
}

/// The channels requests are sent on. Kept separate from the process so tests
/// can drive a session without a real kernel.
struct KernelChannels {
    /// Shell, or control for interrupt and shutdown requests.
    requests: mpsc::Sender<JupyterMessage>,
    stdin: mpsc::Sender<JupyterMessage>,
}

/// Outputs arrive on iopub and the reply on shell, which are separate sockets,
/// so an execution is only finished once both its reply and iopub's idle
/// status for it have arrived.
struct PendingExecution {
    request_id: String,
    succeeded: Option<bool>,
    idle: bool,
}

/// One R process, run by Ark, serving one project window.
pub struct RSession {
    r: Option<RToolchainData>,
    project_root: PathBuf,
    env: HashMap<String, String>,
    bundled_ark: Option<PathBuf>,
    state: SessionState,
    process: Option<ArkProcess>,
    channels: Option<KernelChannels>,
    /// Incremented on every start, so stale exit notifications are ignored.
    generation: usize,
    ui_comm_id: Option<String>,
    lsp_comm_id: Option<String>,
    variables_comm_id: Option<String>,
    data_explorer_comm_ids: Vec<String>,
    plot_comm_ids: Vec<String>,
    /// Kept across restarts, so a new R draws plots at the pane's size too.
    plot_render_settings: Option<PlotRenderSettings>,
    /// Comm RPCs sent and not yet answered, by request message id.
    pending_comm_requests: HashMap<String, oneshot::Sender<Result<serde_json::Value>>>,
    lsp_port: Option<u16>,
    prompt: PromptStateParams,
    working_directory: Option<PathBuf>,
    /// Execute requests sent and not yet finished, oldest first.
    pending_executions: Vec<PendingExecution>,
    pending_replies: HashMap<String, oneshot::Sender<JupyterMessageContent>>,
    pending_input_request: Option<JupyterMessage>,
    _launch_task: Option<Task<()>>,
    _exit_task: Option<Task<()>>,
    /// Socket readers and writers; dropping them disconnects from Ark.
    _io_tasks: Vec<Task<()>>,
    _quit_subscription: Subscription,
    _trust_subscription: Option<Subscription>,
}

impl EventEmitter<SessionEvent> for RSession {}

impl RSession {
    pub fn new(
        r: Option<RToolchainData>,
        project_root: PathBuf,
        env: HashMap<String, String>,
        bundled_ark: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Self {
        let state = if r.is_some() {
            SessionState::Starting
        } else {
            SessionState::Unavailable(
                "R was not found. Install R from https://cran.r-project.org and reopen the project."
                    .to_string(),
            )
        };
        Self {
            r,
            project_root,
            env,
            bundled_ark,
            state,
            process: None,
            channels: None,
            generation: 0,
            ui_comm_id: None,
            lsp_comm_id: None,
            lsp_port: None,
            prompt: PromptStateParams {
                input_prompt: "> ".to_string(),
                continuation_prompt: "+ ".to_string(),
            },
            working_directory: None,
            pending_executions: Vec::new(),
            pending_replies: HashMap::default(),
            variables_comm_id: None,
            data_explorer_comm_ids: Vec::new(),
            plot_comm_ids: Vec::new(),
            plot_render_settings: None,
            pending_comm_requests: HashMap::default(),
            pending_input_request: None,
            _launch_task: None,
            _exit_task: None,
            _io_tasks: Vec::new(),
            _quit_subscription: cx.on_app_quit(Self::shut_down_on_quit),
            _trust_subscription: None,
        }
    }

    pub fn state(&self) -> &SessionState {
        &self.state
    }

    pub fn project_root(&self) -> &PathBuf {
        &self.project_root
    }

    pub fn r_installation(&self) -> Option<&RToolchainData> {
        self.r.as_ref()
    }

    pub fn prompt(&self) -> &PromptStateParams {
        &self.prompt
    }

    pub fn working_directory(&self) -> Option<&PathBuf> {
        self.working_directory.as_ref()
    }

    pub fn language_server_port(&self) -> Option<u16> {
        self.lsp_port
    }

    pub fn variables_comm_id(&self) -> Option<&str> {
        self.variables_comm_id.as_deref()
    }

    /// Sends a JSON-RPC request on one of Ark's comms and resolves to its
    /// result. Ark handles most of these on R's thread, so they wait for R.
    pub fn comm_request(
        &mut self,
        comm_id: &str,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
        cx: &mut Context<Self>,
    ) -> Task<Result<serde_json::Value>> {
        let mut message = JupyterMessage::new(
            CommMsg {
                comm_id: CommId(comm_id.to_string()),
                data: serde_json::Map::new(),
            },
            None,
        );
        let request_id = message.header.msg_id.clone();
        let data = comms::rpc_request_data(&request_id, method, params);
        if let (JupyterMessageContent::CommMsg(comm_message), serde_json::Value::Object(data)) =
            (&mut message.content, data)
        {
            comm_message.data = data;
        }
        if let Err(error) = self.send_request(message) {
            return Task::ready(Err(error));
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        self.pending_comm_requests
            .insert(request_id.clone(), reply_tx);

        let executor = cx.background_executor().clone();
        let method = method.to_string();
        cx.spawn(async move |this, cx| {
            let reply = futures::select_biased! {
                reply = reply_rx.fuse() => Some(reply),
                _ = executor.timer(timeout).fuse() => None,
            };
            match reply {
                Some(Ok(result)) => result,
                Some(Err(_)) => anyhow::bail!("R stopped before answering `{method}`"),
                None => {
                    this.update(cx, |this, _| this.pending_comm_requests.remove(&request_id))
                        .ok();
                    anyhow::bail!(
                        "R didn't answer `{method}` within {} seconds",
                        timeout.as_secs()
                    )
                }
            }
        })
    }

    /// Closes a comm Ark opened, such as a data explorer whose tab closed or a
    /// plot that was deleted; Ark then frees what it held for it.
    pub fn close_comm(&mut self, comm_id: &str) -> Result<()> {
        self.data_explorer_comm_ids.retain(|id| id != comm_id);
        self.plot_comm_ids.retain(|id| id != comm_id);
        self.send_request(JupyterMessage::new(
            CommClose {
                comm_id: CommId(comm_id.to_string()),
                data: serde_json::Map::new(),
            },
            None,
        ))
    }

    /// Draws a plot again, at any size and in any format.
    pub fn render_plot(
        &mut self,
        comm_id: &str,
        params: RenderParams,
        cx: &mut Context<Self>,
    ) -> Task<Result<PlotResult>> {
        let params = match serde_json::to_value(params) {
            Ok(params) => params,
            Err(error) => return Task::ready(Err(error.into())),
        };
        let reply = self.comm_request(comm_id, "render", params, PLOT_REQUEST_TIMEOUT, cx);
        cx.background_spawn(async move {
            serde_json::from_value(reply.await?).context("unexpected reply to `render`")
        })
    }

    pub fn plot_metadata(
        &mut self,
        comm_id: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<PlotMetadata>> {
        let reply = self.comm_request(
            comm_id,
            "get_metadata",
            serde_json::Value::Null,
            PLOT_REQUEST_TIMEOUT,
            cx,
        );
        cx.background_spawn(async move {
            serde_json::from_value(reply.await?).context("unexpected reply to `get_metadata`")
        })
    }

    /// Tells Ark the size to draw new plots at, as when the Plots pane is
    /// resized. Remembered while R isn't running and sent once it starts.
    pub fn set_plot_render_settings(&mut self, settings: PlotRenderSettings) -> Result<()> {
        if self.plot_render_settings == Some(settings) {
            return Ok(());
        }
        self.plot_render_settings = Some(settings);
        match self.ui_comm_id.clone() {
            Some(ui_comm_id) => self.send_plot_render_settings(ui_comm_id, settings),
            None => Ok(()),
        }
    }

    fn send_plot_render_settings(
        &mut self,
        ui_comm_id: String,
        settings: PlotRenderSettings,
    ) -> Result<()> {
        self.send_request(JupyterMessage::new(
            CommMsg {
                comm_id: CommId(ui_comm_id),
                data: json_object(&UiBackendEvent::DidChangePlotsRenderSettings { settings })?,
            },
            None,
        ))
    }

    pub fn is_waiting_for_input(&self) -> bool {
        self.pending_input_request.is_some()
    }

    /// Holds off starting R until the project is trusted; `subscription`
    /// watches for that and calls [`Self::start`].
    pub fn wait_for_trust(&mut self, subscription: Subscription, cx: &mut Context<Self>) {
        if self.r.is_none() {
            return;
        }
        self._trust_subscription = Some(subscription);
        self.set_state(SessionState::Restricted, cx);
    }

    /// Launches Ark. Does nothing when no R installation is available.
    pub fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(r) = self.r.clone() else {
            return;
        };
        self._trust_subscription = None;
        self.generation += 1;
        let generation = self.generation;
        let bundled_ark = self.bundled_ark.clone();
        let env = self.env.clone();
        let working_directory = match launch_directory(&self.project_root) {
            Ok(directory) => directory,
            Err(error) => {
                self.set_state(SessionState::Unavailable(format!("{error}")), cx);
                return;
            }
        };
        self.set_state(SessionState::Starting, cx);

        self._launch_task = Some(cx.spawn_in(window, async move |this, cx| {
            let launched = async {
                let ark = ark::locate_ark(bundled_ark, &env)?;
                log::info!(
                    "starting Ark {} for R {} at {}",
                    ark.display(),
                    r.version.as_deref().unwrap_or("(unknown version)"),
                    r.r_home.display()
                );
                let log_file = ark::next_log_file(&paths::logs_dir().join("ark")).log_err();
                if let Some(log_file) = &log_file {
                    log::info!("Ark logs to {}", log_file.display());
                }
                ark::launch_ark(ArkLaunch {
                    ark: &ark,
                    r_home: &r.r_home,
                    working_directory: &working_directory,
                    env,
                    log_file,
                })
                .await
            }
            .await;

            let (mut process, connections) = match launched {
                Ok(launched) => launched,
                Err(error) => {
                    log::error!("failed to start R: {error:?}");
                    this.update(cx, |this, cx| {
                        this.set_state(
                            SessionState::Exited(format!("R failed to start: {error:#}")),
                            cx,
                        );
                    })
                    .log_err();
                    return;
                }
            };

            let exit_status = process.status();
            let (channels, io_tasks) = spawn_kernel_io(connections, this.clone(), cx);

            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.process = Some(process);
                this._io_tasks = io_tasks;
                this._exit_task = Some(cx.spawn(async move |this, cx| {
                    let message = match exit_status.await {
                        Ok(status) if status.success() => "R exited.".to_string(),
                        Ok(status) => format!("R exited unexpectedly ({status})."),
                        Err(error) => format!("R exited unexpectedly: {error}"),
                    };
                    this.update(cx, |this, cx| {
                        if this.generation == generation {
                            this.handle_exit(message, cx);
                        }
                    })
                    .log_err();
                }));
                this.connect(channels, cx);
            })
            .log_err();
        }));
    }

    /// Stops the current R process, if any, and starts a new one.
    pub fn restart(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.r.is_none() {
            return;
        }
        self.generation += 1;
        self.set_state(SessionState::Restarting, cx);
        let shutdown = self.shut_down(cx);
        self._launch_task = Some(cx.spawn_in(window, async move |this, cx| {
            shutdown.await;
            this.update_in(cx, |this, window, cx| this.start(window, cx))
                .log_err();
        }));
    }

    pub fn execute(&mut self, code: String, cx: &mut Context<Self>) -> Result<String> {
        let message = JupyterMessage::new(
            ExecuteRequest {
                code,
                allow_stdin: true,
                ..ExecuteRequest::default()
            },
            None,
        );
        let request_id = message.header.msg_id.clone();
        self.send_request(message)?;
        self.pending_executions.push(PendingExecution {
            request_id: request_id.clone(),
            succeeded: None,
            idle: false,
        });
        self.set_state(SessionState::Busy, cx);
        Ok(request_id)
    }

    /// Asks R whether `code` is a complete expression. Ark answers on the shell
    /// channel, so the reply waits behind any running execution.
    pub fn is_complete(
        &mut self,
        code: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<IsCompleteReplyStatus>> {
        let message = JupyterMessage::new(IsCompleteRequest { code }, None);
        let reply = match self.send_request_expecting_reply(message) {
            Ok(reply) => reply,
            Err(error) => return Task::ready(Err(error)),
        };
        cx.background_spawn(async move {
            match reply.await.context("R stopped before checking the code")? {
                JupyterMessageContent::IsCompleteReply(reply) => Ok(reply.status),
                other => anyhow::bail!("unexpected reply to is_complete: {}", other.message_type()),
            }
        })
    }

    pub fn send_input(&mut self, value: String) -> Result<()> {
        let request = self
            .pending_input_request
            .take()
            .context("R is not waiting for input")?;
        let reply = JupyterMessage::new(
            InputReply {
                value,
                status: ReplyStatus::Ok,
                error: None,
            },
            Some(&request),
        );
        self.send_stdin(reply)
    }

    pub fn interrupt(&mut self) -> Result<()> {
        self.send_request(JupyterMessage::new(InterruptRequest {}, None))
    }

    fn connect(&mut self, channels: KernelChannels, cx: &mut Context<Self>) {
        self.channels = Some(channels);
        let kernel_info = JupyterMessage::new(KernelInfoRequest {}, None);
        match self.send_request_expecting_reply(kernel_info) {
            Ok(reply) => {
                cx.spawn(async move |this, cx| {
                    if reply.await.is_ok() {
                        this.update(cx, |this, cx| {
                            if this.state == SessionState::Starting {
                                this.set_state(SessionState::Idle, cx);
                            }
                        })
                        .log_err();
                    }
                })
                .detach();
            }
            Err(error) => log::error!("failed to request kernel info: {error:?}"),
        }
        self.open_frontend_comms().log_err();
    }

    fn open_frontend_comms(&mut self) -> Result<()> {
        let ui_comm_id = uuid::Uuid::new_v4().to_string();
        self.send_request(JupyterMessage::new(
            CommOpen {
                comm_id: CommId(ui_comm_id.clone()),
                target_name: UI_COMM_TARGET.to_string(),
                data: json_object(&UiCommOpenData {
                    console_width: DEFAULT_CONSOLE_WIDTH,
                })?,
                target_module: None,
            },
            None,
        ))?;
        self.send_request(JupyterMessage::new(
            CommMsg {
                comm_id: CommId(ui_comm_id.clone()),
                data: json_object(&UiBackendEvent::FrontendReady {
                    start_type: "new".to_string(),
                })?,
            },
            None,
        ))?;
        if let Some(settings) = self.plot_render_settings {
            self.send_plot_render_settings(ui_comm_id.clone(), settings)?;
        }
        self.ui_comm_id = Some(ui_comm_id);

        let lsp_comm_id = uuid::Uuid::new_v4().to_string();
        self.send_request(JupyterMessage::new(
            CommOpen {
                comm_id: CommId(lsp_comm_id.clone()),
                target_name: LSP_COMM_TARGET.to_string(),
                data: json_object(&ServerStartData {
                    ip_address: "127.0.0.1".to_string(),
                })?,
                target_module: None,
            },
            None,
        ))?;
        self.lsp_comm_id = Some(lsp_comm_id);

        // Ark answers by sending a `refresh` with every variable.
        let variables_comm_id = uuid::Uuid::new_v4().to_string();
        self.send_request(JupyterMessage::new(
            CommOpen {
                comm_id: CommId(variables_comm_id.clone()),
                target_name: VARIABLES_COMM_TARGET.to_string(),
                data: serde_json::Map::new(),
                target_module: None,
            },
            None,
        ))?;
        self.variables_comm_id = Some(variables_comm_id);
        Ok(())
    }

    fn handle_message(
        &mut self,
        message: &JupyterMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let parent_id = message
            .parent_header
            .as_ref()
            .map(|header| header.msg_id.clone());
        if let Some(parent_id) = &parent_id
            && message.content.message_type().ends_with("_reply")
            && let Some(reply_tx) = self.pending_replies.remove(parent_id)
        {
            reply_tx.send(message.content.clone()).ok();
        }

        match &message.content {
            JupyterMessageContent::ExecuteInput(_)
            | JupyterMessageContent::StreamContent(_)
            | JupyterMessageContent::DisplayData(_)
            | JupyterMessageContent::UpdateDisplayData(_)
            | JupyterMessageContent::ExecuteResult(_)
            | JupyterMessageContent::ErrorOutput(_)
            | JupyterMessageContent::ClearOutput(_) => {
                cx.emit(SessionEvent::Output {
                    request_id: parent_id,
                    content: message.content.clone(),
                });
            }
            JupyterMessageContent::ExecuteReply(reply) => {
                let Some(request_id) = parent_id else {
                    return;
                };
                // An interrupted `readline()` is never answered.
                self.pending_input_request = None;
                if let Some(execution) = self.pending_execution_mut(&request_id) {
                    execution.succeeded = Some(reply.status == ReplyStatus::Ok);
                }
                self.finish_executions(cx);
            }
            JupyterMessageContent::Status(status)
                if status.execution_state == jupyter_protocol::ExecutionState::Idle =>
            {
                if let Some(request_id) = parent_id
                    && let Some(execution) = self.pending_execution_mut(&request_id)
                {
                    execution.idle = true;
                    self.finish_executions(cx);
                }
            }
            JupyterMessageContent::InputRequest(request) => {
                self.pending_input_request = Some(message.clone());
                cx.emit(SessionEvent::InputRequested {
                    prompt: request.prompt.clone(),
                    password: request.password,
                });
            }
            JupyterMessageContent::CommOpen(open)
                if open.target_name == DATA_EXPLORER_COMM_TARGET =>
            {
                let title = serde_json::from_value::<DataExplorerOpenData>(
                    serde_json::Value::Object(open.data.clone()),
                )
                .map(|data| data.title)
                .unwrap_or_else(|_| "Data".to_string());
                self.data_explorer_comm_ids.push(open.comm_id.0.clone());
                cx.emit(SessionEvent::DataExplorerOpened {
                    comm_id: open.comm_id.0.clone(),
                    title,
                });
            }
            JupyterMessageContent::CommOpen(open) if open.target_name == PLOT_COMM_TARGET => {
                let data = serde_json::from_value::<PlotOpenData>(serde_json::Value::Object(
                    open.data.clone(),
                ))
                .unwrap_or_else(|error| {
                    log::warn!("unexpected data opening a plot comm: {error}");
                    PlotOpenData::default()
                });
                self.plot_comm_ids.push(open.comm_id.0.clone());
                cx.emit(SessionEvent::PlotOpened {
                    comm_id: open.comm_id.0.clone(),
                    pre_render: data.pre_render,
                });
            }
            JupyterMessageContent::CommOpen(open) => {
                log::debug!("ignoring comm opened by Ark: {}", open.target_name);
            }
            JupyterMessageContent::CommMsg(comm_message) => {
                if let Some(parent_id) = &parent_id
                    && let Some(reply_tx) = self.pending_comm_requests.remove(parent_id)
                {
                    let data = serde_json::Value::Object(comm_message.data.clone());
                    reply_tx.send(comms::rpc_reply_result(data)).ok();
                    return;
                }
                self.handle_comm_message(comm_message, cx);
            }
            JupyterMessageContent::CommClose(CommClose { comm_id, .. }) => {
                if self.ui_comm_id.as_deref() == Some(comm_id.0.as_str()) {
                    log::warn!("Ark closed the UI comm");
                    self.ui_comm_id = None;
                }
                if self.lsp_comm_id.as_deref() == Some(comm_id.0.as_str()) {
                    self.lsp_comm_id = None;
                    self.lsp_port = None;
                }
                if self.variables_comm_id.as_deref() == Some(comm_id.0.as_str()) {
                    self.variables_comm_id = None;
                }
                if self.data_explorer_comm_ids.contains(&comm_id.0) {
                    self.data_explorer_comm_ids.retain(|id| *id != comm_id.0);
                    cx.emit(SessionEvent::DataExplorerClosed {
                        comm_id: comm_id.0.clone(),
                    });
                }
                if self.plot_comm_ids.contains(&comm_id.0) {
                    self.plot_comm_ids.retain(|id| *id != comm_id.0);
                    cx.emit(SessionEvent::PlotClosed {
                        comm_id: comm_id.0.clone(),
                    });
                }
            }
            JupyterMessageContent::UnknownMessage(unknown)
                if unknown.msg_type == RPC_REQUEST_MSG_TYPE =>
            {
                self.handle_frontend_request(message, unknown.content.clone(), window, cx);
            }
            _ => {}
        }
    }

    fn pending_execution_mut(&mut self, request_id: &str) -> Option<&mut PendingExecution> {
        self.pending_executions
            .iter_mut()
            .find(|execution| execution.request_id == request_id)
    }

    fn finish_executions(&mut self, cx: &mut Context<Self>) {
        let mut index = 0;
        while index < self.pending_executions.len() {
            let execution = &self.pending_executions[index];
            match (execution.succeeded, execution.idle) {
                (Some(succeeded), true) => {
                    let execution = self.pending_executions.remove(index);
                    cx.emit(SessionEvent::ExecutionFinished {
                        request_id: execution.request_id,
                        succeeded,
                    });
                }
                _ => index += 1,
            }
        }
        if self.pending_executions.is_empty() && self.state == SessionState::Busy {
            self.set_state(SessionState::Idle, cx);
        }
    }

    fn handle_comm_message(&mut self, comm_message: &CommMsg, cx: &mut Context<Self>) {
        let comm_id = comm_message.comm_id.0.as_str();
        let data = serde_json::Value::Object(comm_message.data.clone());
        if self.lsp_comm_id.as_deref() == Some(comm_id) {
            match serde_json::from_value::<LspCommMessage>(data) {
                Ok(LspCommMessage::ServerStarted { port }) => {
                    self.lsp_port = Some(port);
                    cx.emit(SessionEvent::LanguageServerStarted { port });
                }
                Err(error) => log::warn!("unexpected message on Ark's LSP comm: {error}"),
            }
            return;
        }
        if self.variables_comm_id.as_deref() == Some(comm_id) {
            match comms::parse_method::<VariablesEvent>(data) {
                Ok(event) => cx.emit(SessionEvent::Variables(event)),
                Err(error) => log::warn!("unexpected message on Ark's variables comm: {error}"),
            }
            return;
        }
        if self.data_explorer_comm_ids.iter().any(|id| id == comm_id) {
            match comms::parse_method::<DataExplorerEvent>(data) {
                Ok(event) => cx.emit(SessionEvent::DataExplorerEvent {
                    comm_id: comm_id.to_string(),
                    event,
                }),
                Err(error) => log::warn!("unexpected message on a data explorer comm: {error}"),
            }
            return;
        }
        if self.plot_comm_ids.iter().any(|id| id == comm_id) {
            match comms::parse_method::<PlotEvent>(data) {
                Ok(event) => cx.emit(SessionEvent::PlotEvent {
                    comm_id: comm_id.to_string(),
                    event,
                }),
                Err(error) => log::warn!("unexpected message on a plot comm: {error}"),
            }
            return;
        }
        if self.ui_comm_id.as_deref() != Some(comm_id) {
            return;
        }
        let event = match comms::parse_method::<UiFrontendEvent>(data) {
            Ok(event) => event,
            Err(error) => {
                log::warn!("unexpected message on Ark's UI comm: {error}");
                return;
            }
        };
        match event {
            UiFrontendEvent::PromptState(prompt) => {
                self.prompt = prompt;
                cx.emit(SessionEvent::PromptChanged);
            }
            UiFrontendEvent::WorkingDirectory(params) => {
                self.working_directory = Some(PathBuf::from(params.directory));
                cx.emit(SessionEvent::WorkingDirectoryChanged);
            }
            UiFrontendEvent::ClearConsole => cx.emit(SessionEvent::ClearConsole),
            UiFrontendEvent::ShowMessage(params) => {
                cx.emit(SessionEvent::ShowMessage(params.message));
            }
            UiFrontendEvent::OpenEditor(params) => cx.emit(SessionEvent::OpenEditor {
                path: PathBuf::from(params.file),
                line: u32::try_from(params.line).unwrap_or(0),
                column: u32::try_from(params.column).unwrap_or(0),
            }),
            UiFrontendEvent::ShowUrl(params) => cx.open_url(&params.url),
            UiFrontendEvent::ShowHtmlFile(params) => {
                cx.open_with_system(&PathBuf::from(params.path));
            }
            UiFrontendEvent::OpenWithSystem(params) => {
                cx.open_with_system(&PathBuf::from(params.path));
            }
            UiFrontendEvent::Busy(_)
            | UiFrontendEvent::OpenWorkspace(_)
            | UiFrontendEvent::SetEditorSelections(_)
            | UiFrontendEvent::ClearWebviewPreloads => {}
        }
    }

    /// Answers a request R is blocked on. Every path must send a reply, or R
    /// waits forever.
    fn handle_frontend_request(
        &mut self,
        message: &JupyterMessage,
        content: serde_json::Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request = match comms::parse_method::<UiFrontendRequest>(content) {
            Ok(request) => request,
            Err(error) => {
                self.reply_to_frontend_request(
                    message,
                    comms::rpc_error(format!(
                        "RezStudio does not understand this request: {error}"
                    )),
                )
                .log_err();
                return;
            }
        };

        let unsupported =
            |method: &str| comms::rpc_error(format!("RezStudio does not support `{method}` yet."));
        let reply = match request {
            UiFrontendRequest::WorkspaceFolder => comms::rpc_result(serde_json::Value::String(
                launch_directory(&self.project_root)
                    .unwrap_or_else(|_| self.project_root.clone())
                    .to_string_lossy()
                    .into_owned(),
            )),
            UiFrontendRequest::LastActiveEditorContext => {
                comms::rpc_result(serde_json::Value::Null)
            }
            UiFrontendRequest::EvaluateWhenClause(_) => {
                comms::rpc_result(serde_json::Value::Bool(false))
            }
            UiFrontendRequest::ShowQuestion(params) => {
                let answer = window.prompt(
                    PromptLevel::Info,
                    &params.title,
                    Some(&params.message),
                    &[
                        params.ok_button_title.as_str(),
                        params.cancel_button_title.as_str(),
                    ],
                    cx,
                );
                self.reply_when_answered(
                    message.clone(),
                    answer,
                    |answer| serde_json::Value::Bool(answer == Some(0)),
                    cx,
                );
                return;
            }
            UiFrontendRequest::ShowDialog(params) => {
                let answer = window.prompt(
                    PromptLevel::Info,
                    &params.title,
                    Some(&params.message),
                    &["OK"],
                    cx,
                );
                self.reply_when_answered(message.clone(), answer, |_| serde_json::Value::Null, cx);
                return;
            }
            UiFrontendRequest::DebugSleep(params) => {
                let request = message.clone();
                let duration = Duration::from_secs_f64(params.ms.max(0.0) / 1000.0);
                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(duration).await;
                    this.update(cx, |this, _| {
                        this.reply_to_frontend_request(
                            &request,
                            comms::rpc_result(serde_json::Value::Null),
                        )
                    })
                    .log_err();
                })
                .detach();
                return;
            }
            UiFrontendRequest::ShowPrompt(_) => unsupported("show_prompt"),
            UiFrontendRequest::AskForPassword(_) => unsupported("ask_for_password"),
            UiFrontendRequest::NewDocument(_) => unsupported("new_document"),
            UiFrontendRequest::ExecuteCommand(_) => unsupported("execute_command"),
            UiFrontendRequest::ExecuteCode(_) => unsupported("execute_code"),
            UiFrontendRequest::ModifyEditorSelections(_) => unsupported("modify_editor_selections"),
        };
        self.reply_to_frontend_request(message, reply).log_err();
    }

    fn reply_when_answered(
        &mut self,
        request: JupyterMessage,
        answer: oneshot::Receiver<usize>,
        to_result: impl FnOnce(Option<usize>) -> serde_json::Value + 'static,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let answer = answer.await.ok();
            this.update(cx, |this, _| {
                this.reply_to_frontend_request(&request, comms::rpc_result(to_result(answer)))
            })
            .log_err();
        })
        .detach();
    }

    fn reply_to_frontend_request(
        &mut self,
        request: &JupyterMessage,
        reply: serde_json::Value,
    ) -> Result<()> {
        let reply = JupyterMessage::new(
            UnknownMessage {
                msg_type: RPC_REPLY_MSG_TYPE.to_string(),
                content: reply,
            },
            Some(request),
        );
        self.send_stdin(reply)
    }

    fn handle_exit(&mut self, message: String, cx: &mut Context<Self>) {
        log::info!("{message}");
        self.process = None;
        self.forget_kernel_state(cx);
        self.set_state(SessionState::Exited(message), cx);
    }

    /// Drops everything tied to the R process that's going away. Pending
    /// requests fail, executions finish unsuccessfully and viewers close.
    fn forget_kernel_state(&mut self, cx: &mut Context<Self>) {
        self.channels = None;
        self._io_tasks.clear();
        self.ui_comm_id = None;
        self.lsp_comm_id = None;
        self.lsp_port = None;
        self.variables_comm_id = None;
        self.pending_input_request = None;
        self.pending_replies.clear();
        self.pending_comm_requests.clear();
        for execution in std::mem::take(&mut self.pending_executions) {
            cx.emit(SessionEvent::ExecutionFinished {
                request_id: execution.request_id,
                succeeded: false,
            });
        }
        for comm_id in std::mem::take(&mut self.data_explorer_comm_ids) {
            cx.emit(SessionEvent::DataExplorerClosed { comm_id });
        }
        for comm_id in std::mem::take(&mut self.plot_comm_ids) {
            cx.emit(SessionEvent::PlotClosed { comm_id });
        }
    }

    /// Stops R for good, as when its window closes. The returned task finishes
    /// once Ark has exited or been killed.
    pub fn stop(&mut self, cx: &mut Context<Self>) -> Task<()> {
        self.generation += 1;
        let shutdown = self.shut_down(cx);
        if self.r.is_some() {
            self.set_state(SessionState::Exited("R was stopped.".to_string()), cx);
        }
        shutdown
    }

    /// Asks Ark to shut down, then kills it if it hasn't exited in time.
    fn shut_down(&mut self, cx: &mut Context<Self>) -> Task<()> {
        let shutdown = JupyterMessage::new(ShutdownRequest { restart: false }, None);
        if self.send_request(shutdown).is_err() {
            self.process = None;
        }
        self.forget_kernel_state(cx);
        self._exit_task = None;
        let Some(mut process) = self.process.take() else {
            return Task::ready(());
        };
        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            let exited = process.status();
            let timeout = executor.timer(SHUTDOWN_GRACE_PERIOD);
            futures::future::select(Box::pin(exited), timeout).await;
            drop(process);
        })
    }

    fn shut_down_on_quit(&mut self, cx: &mut Context<Self>) -> impl Future<Output = ()> + use<> {
        self.generation += 1;
        self.shut_down(cx)
    }

    fn set_state(&mut self, state: SessionState, cx: &mut Context<Self>) {
        if self.state != state {
            self.state = state;
            cx.emit(SessionEvent::StateChanged);
            cx.notify();
        }
    }

    fn send_request(&mut self, message: JupyterMessage) -> Result<()> {
        let channels = self.channels.as_mut().context("R is not running")?;
        channels
            .requests
            .try_send(message)
            .context("sending a request to R")
    }

    fn send_stdin(&mut self, message: JupyterMessage) -> Result<()> {
        let channels = self.channels.as_mut().context("R is not running")?;
        channels
            .stdin
            .try_send(message)
            .context("sending input to R")
    }

    fn send_request_expecting_reply(
        &mut self,
        message: JupyterMessage,
    ) -> Result<oneshot::Receiver<JupyterMessageContent>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let request_id = message.header.msg_id.clone();
        self.send_request(message)?;
        self.pending_replies.insert(request_id, reply_tx);
        Ok(reply_rx)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn connect_for_test(
        &mut self,
        requests: mpsc::Sender<JupyterMessage>,
        stdin: mpsc::Sender<JupyterMessage>,
        cx: &mut Context<Self>,
    ) {
        self.connect(KernelChannels { requests, stdin }, cx);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn receive_for_test(
        &mut self,
        message: &JupyterMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.handle_message(message, window, cx);
    }
}

/// The folder R runs in: the project folder, or the folder containing the
/// file when a single file was opened as the project.
fn launch_directory(project_root: &std::path::Path) -> Result<PathBuf> {
    if project_root.is_dir() {
        return Ok(project_root.to_path_buf());
    }
    if let Some(parent) = project_root.parent()
        && project_root.is_file()
    {
        return Ok(parent.to_path_buf());
    }
    anyhow::bail!(
        "R can't start because the project folder {} doesn't exist.",
        project_root.display()
    )
}

fn json_object(
    value: &impl serde::Serialize,
) -> Result<serde_json::Map<String, serde_json::Value>> {
    match serde_json::to_value(value)? {
        serde_json::Value::Object(object) => Ok(object),
        other => anyhow::bail!("expected a JSON object, got {other}"),
    }
}

/// Reads Ark's sockets into the session and writes queued requests out. The
/// session is held weakly, so dropping it ends these tasks and disconnects.
fn spawn_kernel_io(
    connections: crate::ark::ArkConnections,
    session: WeakEntity<RSession>,
    cx: &mut AsyncWindowContext,
) -> (KernelChannels, Vec<Task<()>>) {
    let crate::ark::ArkConnections {
        mut iopub,
        shell,
        control,
        stdin,
    } = connections;
    let (mut shell_send, mut shell_recv) = shell.split();
    let (mut control_send, mut control_recv) = control.split();
    let (mut stdin_send, mut stdin_recv) = stdin.split();
    let (requests_tx, mut requests_rx) = mpsc::channel::<JupyterMessage>(100);
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<JupyterMessage>(100);

    let receive = cx.spawn(async move |cx| {
        loop {
            let (channel, result) = futures::select! {
                message = iopub.read().fuse() => ("iopub", message),
                message = shell_recv.read().fuse() => ("shell", message),
                message = control_recv.read().fuse() => ("control", message),
                message = stdin_recv.read().fuse() => ("stdin", message),
            };
            match result {
                Ok(message) => {
                    let delivered = session.update_in(cx, |session, window, cx| {
                        session.handle_message(&message, window, cx)
                    });
                    if delivered.is_err() {
                        break;
                    }
                }
                Err(
                    error @ (runtimelib::RuntimeError::ParseError { .. }
                    | runtimelib::RuntimeError::SerdeError(_)),
                ) => log::warn!("unreadable message from Ark on {channel}: {error}"),
                Err(error) => {
                    log::error!("lost connection to Ark on {channel}: {error}");
                    break;
                }
            }
        }
    });

    let send_requests = cx.background_spawn(async move {
        while let Some(message) = requests_rx.next().await {
            let sent = match message.content {
                JupyterMessageContent::InterruptRequest(_)
                | JupyterMessageContent::ShutdownRequest(_)
                | JupyterMessageContent::DebugRequest(_) => control_send.send(message).await,
                _ => shell_send.send(message).await,
            };
            if let Err(error) = sent {
                log::error!("failed to send a request to Ark: {error}");
                break;
            }
        }
    });

    let send_stdin = cx.background_spawn(async move {
        while let Some(message) = stdin_rx.next().await {
            if let Err(error) = stdin_send.send(message).await {
                log::error!("failed to send input to Ark: {error}");
                break;
            }
        }
    });

    (
        KernelChannels {
            requests: requests_tx,
            stdin: stdin_tx,
        },
        vec![receive, send_requests, send_stdin],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use jupyter_protocol::{
        ExecuteReply, InputRequest, IsCompleteReply, KernelInfoReply, Status, Stdio, StreamContent,
    };
    use languages::r::RInstallationSource;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::cell::RefCell;
    use std::rc::Rc;

    struct Harness {
        session: Entity<RSession>,
        requests: mpsc::Receiver<JupyterMessage>,
        stdin: mpsc::Receiver<JupyterMessage>,
        events: Rc<RefCell<Vec<SessionEvent>>>,
    }

    impl Harness {
        fn new(cx: &mut VisualTestContext) -> Self {
            let r = RToolchainData {
                r_home: PathBuf::from("/opt/R/4.5.3/lib/R"),
                version: Some("4.5.3".to_string()),
                source: RInstallationSource::Rig,
            };
            let session = cx.new(|cx| {
                RSession::new(
                    Some(r),
                    PathBuf::from("/projects/analysis"),
                    HashMap::default(),
                    None,
                    cx,
                )
            });
            let events = Rc::new(RefCell::new(Vec::new()));
            cx.update(|_, cx| {
                let events = events.clone();
                cx.subscribe(&session, move |_, event, _| {
                    events.borrow_mut().push(event.clone());
                })
                .detach();
            });
            let (requests_tx, requests) = mpsc::channel(100);
            let (stdin_tx, stdin) = mpsc::channel(100);
            session.update(cx, |session, cx| {
                session.connect_for_test(requests_tx, stdin_tx, cx)
            });
            Self {
                session,
                requests,
                stdin,
                events,
            }
        }

        fn next_request(&mut self) -> JupyterMessage {
            self.requests.try_recv().expect("expected a request to Ark")
        }

        fn next_stdin(&mut self) -> JupyterMessage {
            self.stdin.try_recv().expect("expected a message on stdin")
        }

        fn receive(&self, message: JupyterMessage, cx: &mut VisualTestContext) {
            self.session.update_in(cx, |session, window, cx| {
                session.receive_for_test(&message, window, cx)
            });
        }

        fn take_events(&self) -> Vec<SessionEvent> {
            std::mem::take(&mut *self.events.borrow_mut())
        }

        /// Consumes the startup requests and returns the UI and LSP comm ids.
        fn finish_startup(&mut self, cx: &mut VisualTestContext) -> (CommId, CommId) {
            let kernel_info = self.next_request();
            let ui_open = self.next_request();
            let _frontend_ready = self.next_request();
            let lsp_open = self.next_request();
            let _variables_open = self.next_request();
            self.receive(
                JupyterMessage::new(kernel_info_reply(), Some(&kernel_info)),
                cx,
            );
            cx.run_until_parked();
            let comm_id = |message: &JupyterMessage| match &message.content {
                JupyterMessageContent::CommOpen(open) => open.comm_id.clone(),
                other => panic!("expected comm_open, got {}", other.message_type()),
            };
            (comm_id(&ui_open), comm_id(&lsp_open))
        }
    }

    fn kernel_info_reply() -> KernelInfoReply {
        serde_json::from_value(json!({
            "status": "ok",
            "protocol_version": "5.3",
            "implementation": "ark",
            "implementation_version": "0.1.252",
            "language_info": { "name": "R", "version": "4.5.3" },
            "banner": "",
            "help_links": [],
            "debugger": false
        }))
        .unwrap()
    }

    fn comm_message(comm_id: &CommId, data: serde_json::Value) -> JupyterMessage {
        let serde_json::Value::Object(data) = data else {
            panic!("comm data must be an object");
        };
        JupyterMessage::new(
            CommMsg {
                comm_id: comm_id.clone(),
                data,
            },
            None,
        )
    }

    fn rpc_request(content: serde_json::Value) -> JupyterMessage {
        JupyterMessage::new(
            UnknownMessage {
                msg_type: RPC_REQUEST_MSG_TYPE.to_string(),
                content,
            },
            None,
        )
    }

    fn rpc_reply_content(message: &JupyterMessage, request: &JupyterMessage) -> serde_json::Value {
        assert_eq!(message.header.msg_type, RPC_REPLY_MSG_TYPE);
        assert_eq!(
            message.parent_header.as_ref().map(|header| &header.msg_id),
            Some(&request.header.msg_id)
        );
        match &message.content {
            JupyterMessageContent::UnknownMessage(unknown) => unknown.content.clone(),
            other => panic!("expected rpc_reply, got {}", other.message_type()),
        }
    }

    #[gpui::test]
    fn test_startup_opens_comms_and_becomes_idle(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let mut harness = Harness::new(cx);

        let kernel_info = harness.next_request();
        assert!(matches!(
            kernel_info.content,
            JupyterMessageContent::KernelInfoRequest(_)
        ));

        let ui_open = harness.next_request();
        let JupyterMessageContent::CommOpen(open) = &ui_open.content else {
            panic!("expected the UI comm to open");
        };
        assert_eq!(open.target_name, UI_COMM_TARGET);
        assert_eq!(
            serde_json::Value::Object(open.data.clone()),
            json!({ "console_width": 80 })
        );

        let frontend_ready = harness.next_request();
        let JupyterMessageContent::CommMsg(ready) = &frontend_ready.content else {
            panic!("expected frontend_ready");
        };
        assert_eq!(ready.comm_id, open.comm_id);
        assert_eq!(
            serde_json::Value::Object(ready.data.clone()),
            json!({ "method": "frontend_ready", "params": { "start_type": "new" } })
        );

        let lsp_open = harness.next_request();
        let JupyterMessageContent::CommOpen(lsp) = &lsp_open.content else {
            panic!("expected the LSP comm to open");
        };
        assert_eq!(lsp.target_name, LSP_COMM_TARGET);
        assert_eq!(
            serde_json::Value::Object(lsp.data.clone()),
            json!({ "ip_address": "127.0.0.1" })
        );

        let variables_open = harness.next_request();
        let JupyterMessageContent::CommOpen(variables) = &variables_open.content else {
            panic!("expected the variables comm to open");
        };
        assert_eq!(variables.target_name, VARIABLES_COMM_TARGET);
        assert_eq!(
            harness.session.read_with(cx, |session, _| session
                .variables_comm_id()
                .map(str::to_string)),
            Some(variables.comm_id.0.clone())
        );

        assert_eq!(
            harness
                .session
                .read_with(cx, |session, _| session.state().clone()),
            SessionState::Starting
        );
        harness.receive(
            JupyterMessage::new(kernel_info_reply(), Some(&kernel_info)),
            cx,
        );
        cx.run_until_parked();
        assert_eq!(
            harness
                .session
                .read_with(cx, |session, _| session.state().clone()),
            SessionState::Idle
        );
    }

    #[gpui::test]
    fn test_execution_outputs_and_completion(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let mut harness = Harness::new(cx);
        harness.finish_startup(cx);
        harness.take_events();

        let request_id = harness
            .session
            .update(cx, |session, cx| session.execute("print(1:3)".into(), cx))
            .unwrap();
        let request = harness.next_request();
        let JupyterMessageContent::ExecuteRequest(execute) = &request.content else {
            panic!("expected execute_request");
        };
        assert_eq!(execute.code, "print(1:3)");
        assert!(execute.allow_stdin);
        assert_eq!(
            harness
                .session
                .read_with(cx, |session, _| session.state().clone()),
            SessionState::Busy
        );

        // Ark's reply can overtake output still in flight on iopub.
        harness.receive(
            JupyterMessage::new(ExecuteReply::default(), Some(&request)),
            cx,
        );
        harness.receive(
            JupyterMessage::new(
                StreamContent {
                    name: Stdio::Stdout,
                    text: "[1] 1 2 3\n".into(),
                },
                Some(&request),
            ),
            cx,
        );
        assert_eq!(
            harness
                .session
                .read_with(cx, |session, _| session.state().clone()),
            SessionState::Busy
        );
        harness.receive(JupyterMessage::new(Status::idle(), Some(&request)), cx);

        let events = harness.take_events();
        assert!(matches!(
            &events[..],
            [
                SessionEvent::StateChanged,
                SessionEvent::Output { request_id: Some(output_id), content: JupyterMessageContent::StreamContent(_) },
                SessionEvent::ExecutionFinished { request_id: finished_id, succeeded: true },
                SessionEvent::StateChanged,
            ] if *output_id == request_id && *finished_id == request_id
        ));
        assert_eq!(
            harness
                .session
                .read_with(cx, |session, _| session.state().clone()),
            SessionState::Idle
        );
    }

    #[gpui::test]
    fn test_ui_and_lsp_comm_events(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let mut harness = Harness::new(cx);
        let (ui_comm, lsp_comm) = harness.finish_startup(cx);
        harness.take_events();

        harness.receive(
            comm_message(
                &ui_comm,
                json!({
                    "method": "prompt_state",
                    "params": { "input_prompt": "Browse[1]> ", "continuation_prompt": "+ " }
                }),
            ),
            cx,
        );
        harness.receive(
            comm_message(
                &ui_comm,
                json!({ "method": "working_directory", "params": { "directory": "/projects/analysis/data" } }),
            ),
            cx,
        );
        harness.receive(
            comm_message(
                &lsp_comm,
                json!({ "msg_type": "server_started", "content": { "port": 40123 } }),
            ),
            cx,
        );
        // Messages on comms that were never opened are ignored.
        harness.receive(
            comm_message(
                &CommId("plot".into()),
                json!({ "method": "update", "params": {} }),
            ),
            cx,
        );

        harness.session.read_with(cx, |session, _| {
            assert_eq!(session.prompt().input_prompt, "Browse[1]> ");
            assert_eq!(
                session.working_directory(),
                Some(&PathBuf::from("/projects/analysis/data"))
            );
            assert_eq!(session.language_server_port(), Some(40123));
        });
        assert!(matches!(
            &harness.take_events()[..],
            [
                SessionEvent::PromptChanged,
                SessionEvent::WorkingDirectoryChanged,
                SessionEvent::LanguageServerStarted { port: 40123 },
            ]
        ));
    }

    #[gpui::test]
    fn test_frontend_requests_are_always_answered(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let mut harness = Harness::new(cx);
        harness.finish_startup(cx);

        let request = rpc_request(json!({ "method": "workspace_folder", "params": {} }));
        harness.receive(request.clone(), cx);
        let reply = harness.next_stdin();
        assert_eq!(
            rpc_reply_content(&reply, &request),
            json!({ "result": "/projects/analysis" })
        );

        let request = rpc_request(
            json!({ "method": "ask_for_password", "params": { "prompt": "Password:" } }),
        );
        harness.receive(request.clone(), cx);
        let reply = harness.next_stdin();
        let content = rpc_reply_content(&reply, &request);
        assert!(
            content["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("ask_for_password"))
        );

        let request = rpc_request(json!({ "method": "not_a_method", "params": {} }));
        harness.receive(request.clone(), cx);
        let reply = harness.next_stdin();
        assert!(rpc_reply_content(&reply, &request).get("error").is_some());

        let request = rpc_request(json!({
            "method": "show_question",
            "params": {
                "title": "Install packages",
                "message": "Install 2 packages?",
                "ok_button_title": "Install",
                "cancel_button_title": "Cancel"
            }
        }));
        harness.receive(request.clone(), cx);
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Install");
        cx.run_until_parked();
        let reply = harness.next_stdin();
        assert_eq!(
            rpc_reply_content(&reply, &request),
            json!({ "result": true })
        );
    }

    #[gpui::test]
    fn test_input_requests(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let mut harness = Harness::new(cx);
        harness.finish_startup(cx);
        harness.take_events();

        let input_request = JupyterMessage::new(
            InputRequest {
                prompt: "Pick one: ".into(),
                password: false,
            },
            None,
        );
        harness.receive(input_request.clone(), cx);
        assert!(matches!(
            &harness.take_events()[..],
            [SessionEvent::InputRequested { prompt, password: false }] if prompt == "Pick one: "
        ));
        assert!(
            harness
                .session
                .read_with(cx, |session, _| session.is_waiting_for_input())
        );

        harness
            .session
            .update(cx, |session, _| session.send_input("2".into()))
            .unwrap();
        let reply = harness.next_stdin();
        let JupyterMessageContent::InputReply(input) = &reply.content else {
            panic!("expected input_reply");
        };
        assert_eq!(input.value, "2");
        assert_eq!(
            reply.parent_header.map(|header| header.msg_id),
            Some(input_request.header.msg_id)
        );
        assert!(
            harness
                .session
                .update(cx, |session, _| session.send_input("again".into()))
                .is_err()
        );
    }

    #[gpui::test]
    fn test_is_complete(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let mut harness = Harness::new(cx);
        harness.finish_startup(cx);

        let status = harness
            .session
            .update(cx, |session, cx| session.is_complete("mean(".into(), cx));
        let request = harness.next_request();
        assert!(matches!(
            request.content,
            JupyterMessageContent::IsCompleteRequest(_)
        ));
        harness.receive(
            JupyterMessage::new(
                IsCompleteReply::new(IsCompleteReplyStatus::Incomplete, String::new()),
                Some(&request),
            ),
            cx,
        );
        let status = cx.foreground_executor().block_test(status).unwrap();
        assert_eq!(status, IsCompleteReplyStatus::Incomplete);
    }

    fn variables_comm(harness: &Harness, cx: &mut VisualTestContext) -> CommId {
        CommId(harness.session.read_with(cx, |session, _| {
            session.variables_comm_id().unwrap().to_string()
        }))
    }

    #[gpui::test]
    fn test_comm_requests_resolve_with_their_reply(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let mut harness = Harness::new(cx);
        harness.finish_startup(cx);
        let comm_id = variables_comm(&harness, cx);

        let inspect = harness.session.update(cx, |session, cx| {
            session.comm_request(
                &comm_id.0,
                "inspect",
                json!({ "path": ["df"] }),
                Duration::from_secs(5),
                cx,
            )
        });
        let request = harness.next_request();
        let JupyterMessageContent::CommMsg(comm_message) = &request.content else {
            panic!("expected comm_msg");
        };
        assert_eq!(comm_message.comm_id, comm_id);
        assert_eq!(
            serde_json::Value::Object(comm_message.data.clone()),
            json!({ "jsonrpc": "2.0", "id": request.header.msg_id, "method": "inspect", "params": { "path": ["df"] } })
        );
        // Ark replies on iopub as a comm_msg parented to the request.
        harness.receive(
            JupyterMessage::new(
                CommMsg {
                    comm_id: comm_id.clone(),
                    data: json_object(
                        &json!({ "method": "inspect", "result": { "children": [], "length": 0 } }),
                    )
                    .unwrap(),
                },
                Some(&request),
            ),
            cx,
        );
        let result = cx.foreground_executor().block_test(inspect).unwrap();
        assert_eq!(result, json!({ "children": [], "length": 0 }));

        let failing = harness.session.update(cx, |session, cx| {
            session.comm_request(
                &comm_id.0,
                "view",
                json!({ "path": ["nope"] }),
                Duration::from_secs(5),
                cx,
            )
        });
        let request = harness.next_request();
        harness.receive(
            JupyterMessage::new(
                CommMsg {
                    comm_id: comm_id.clone(),
                    data: json_object(&json!({ "error": { "code": -32603, "message": "object 'nope' not found" } }))
                        .unwrap(),
                },
                Some(&request),
            ),
            cx,
        );
        let error = cx.foreground_executor().block_test(failing).unwrap_err();
        assert!(error.to_string().contains("not found"), "{error}");

        let unanswered = harness.session.update(cx, |session, cx| {
            session.comm_request(&comm_id.0, "list", json!({}), Duration::from_secs(5), cx)
        });
        harness.next_request();
        cx.executor().advance_clock(Duration::from_secs(6));
        let error = cx.foreground_executor().block_test(unanswered).unwrap_err();
        assert!(
            error.to_string().contains("didn't answer `list`"),
            "{error}"
        );
    }

    #[gpui::test]
    fn test_variables_events_are_forwarded(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let mut harness = Harness::new(cx);
        harness.finish_startup(cx);
        let comm_id = variables_comm(&harness, cx);
        harness.take_events();

        harness.receive(
            comm_message(
                &comm_id,
                json!({ "method": "refresh", "params": { "variables": [], "length": 0, "version": 1 } }),
            ),
            cx,
        );
        assert!(matches!(
            &harness.take_events()[..],
            [SessionEvent::Variables(VariablesEvent::Refresh(refresh))] if refresh.version == 1
        ));
    }

    #[gpui::test]
    fn test_plot_comms(cx: &mut TestAppContext) {
        use crate::comms::{PlotRenderFormat, PlotSize};

        let cx = cx.add_empty_window();
        let mut harness = Harness::new(cx);
        let (ui_comm, _) = harness.finish_startup(cx);
        harness.take_events();

        // The pane's size reaches Ark as a UI comm event, and only when it changes.
        let settings = PlotRenderSettings {
            size: PlotSize {
                width: 500,
                height: 300,
            },
            pixel_ratio: 2.0,
            format: PlotRenderFormat::Png,
        };
        let settings_event = json!({
            "method": "did_change_plots_render_settings",
            "params": { "settings": {
                "size": { "width": 500, "height": 300 }, "pixel_ratio": 2.0, "format": "png"
            }}
        });
        harness
            .session
            .update(cx, |session, _| session.set_plot_render_settings(settings))
            .unwrap();
        let request = harness.next_request();
        assert!(matches!(
            &request.content,
            JupyterMessageContent::CommMsg(message)
                if message.comm_id == ui_comm && serde_json::Value::Object(message.data.clone()) == settings_event
        ));
        harness
            .session
            .update(cx, |session, _| session.set_plot_render_settings(settings))
            .unwrap();
        assert!(harness.requests.try_recv().is_err());

        let plot = CommId("plot-1".to_string());
        let pre_render = json!({ "data": "aGVsbG8", "mime_type": "image/png", "settings": null });
        harness.receive(
            JupyterMessage::new(
                CommOpen {
                    comm_id: plot.clone(),
                    target_name: PLOT_COMM_TARGET.to_string(),
                    data: json_object(&json!({ "pre_render": pre_render })).unwrap(),
                    target_module: None,
                },
                None,
            ),
            cx,
        );
        harness.receive(
            comm_message(
                &plot,
                json!({ "method": "update", "params": { "pre_render": pre_render } }),
            ),
            cx,
        );
        let events = harness.take_events();
        let [
            SessionEvent::PlotOpened {
                comm_id,
                pre_render: Some(opened),
            },
            SessionEvent::PlotEvent {
                event:
                    PlotEvent::Update {
                        pre_render: Some(_),
                    },
                ..
            },
        ] = &events[..]
        else {
            panic!("unexpected events {events:?}");
        };
        assert_eq!(comm_id, "plot-1");
        assert_eq!(opened.decode().unwrap(), b"hello");

        let render = harness.session.update(cx, |session, cx| {
            session.render_plot(
                "plot-1",
                RenderParams {
                    size: Some(PlotSize {
                        width: 800,
                        height: 600,
                    }),
                    pixel_ratio: 2.0,
                    format: PlotRenderFormat::Svg,
                },
                cx,
            )
        });
        let request = harness.next_request();
        let JupyterMessageContent::CommMsg(message) = &request.content else {
            panic!("expected comm_msg");
        };
        assert_eq!(message.data["method"], "render");
        assert_eq!(
            message.data["params"],
            json!({ "size": { "width": 800, "height": 600 }, "pixel_ratio": 2.0, "format": "svg" })
        );
        harness.receive(
            JupyterMessage::new(
                CommMsg {
                    comm_id: plot.clone(),
                    data: json_object(&json!({ "method": "render", "result": {
                        "data": "PHN2Zz4", "mime_type": "image/svg+xml", "settings": null
                    }}))
                    .unwrap(),
                },
                Some(&request),
            ),
            cx,
        );
        let rendered = cx.foreground_executor().block_test(render).unwrap();
        assert_eq!(rendered.mime_type, "image/svg+xml");
        assert_eq!(rendered.decode().unwrap(), b"<svg>");

        // Deleting a plot closes its comm; Ark's later messages on it are ignored.
        harness
            .session
            .update(cx, |session, _| session.close_comm("plot-1"))
            .unwrap();
        assert!(matches!(
            &harness.next_request().content,
            JupyterMessageContent::CommClose(close) if close.comm_id == plot
        ));
        harness.receive(
            comm_message(&plot, json!({ "method": "show", "params": {} })),
            cx,
        );
        assert!(harness.take_events().is_empty());

        // Plots still open when R stops are reported closed, and a new R is
        // told the pane's size as soon as the UI comm opens.
        harness.receive(
            JupyterMessage::new(
                CommOpen {
                    comm_id: CommId("plot-2".to_string()),
                    target_name: PLOT_COMM_TARGET.to_string(),
                    data: serde_json::Map::new(),
                    target_module: None,
                },
                None,
            ),
            cx,
        );
        harness.session.update(cx, |session, cx| {
            session.handle_exit("R exited.".into(), cx)
        });
        let events = harness.take_events();
        assert!(matches!(
            &events[0],
            SessionEvent::PlotOpened {
                pre_render: None,
                ..
            }
        ));
        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::PlotClosed { comm_id } if comm_id == "plot-2"
        )));

        let (requests_tx, requests) = mpsc::channel(100);
        let (stdin_tx, _stdin) = mpsc::channel(100);
        harness.requests = requests;
        harness.session.update(cx, |session, cx| {
            session.connect_for_test(requests_tx, stdin_tx, cx)
        });
        let methods: Vec<String> = std::iter::from_fn(|| harness.requests.try_recv().ok())
            .map(|request| match &request.content {
                JupyterMessageContent::CommMsg(message) => message.data["method"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                other => other.message_type().to_string(),
            })
            .collect();
        assert_eq!(
            methods,
            [
                "kernel_info_request",
                "comm_open",
                "frontend_ready",
                "did_change_plots_render_settings",
                "comm_open",
                "comm_open",
            ]
        );
    }

    #[gpui::test]
    fn test_data_explorer_comm_lifecycle(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let mut harness = Harness::new(cx);
        harness.finish_startup(cx);
        harness.take_events();

        let explorer = CommId("explorer-1".to_string());
        harness.receive(
            JupyterMessage::new(
                CommOpen {
                    comm_id: explorer.clone(),
                    target_name: DATA_EXPLORER_COMM_TARGET.to_string(),
                    data: json_object(&json!({ "title": "mtcars", "inline_only": false })).unwrap(),
                    target_module: None,
                },
                None,
            ),
            cx,
        );
        harness.receive(
            comm_message(&explorer, json!({ "method": "data_update", "params": {} })),
            cx,
        );
        assert!(matches!(
            &harness.take_events()[..],
            [
                SessionEvent::DataExplorerOpened { comm_id, title },
                SessionEvent::DataExplorerEvent { event: DataExplorerEvent::DataUpdate, .. },
            ] if comm_id == "explorer-1" && title == "mtcars"
        ));

        harness
            .session
            .update(cx, |session, _| session.close_comm("explorer-1"))
            .unwrap();
        let close = harness.next_request();
        assert!(matches!(
            &close.content,
            JupyterMessageContent::CommClose(close) if close.comm_id == explorer
        ));

        // An explorer still open when R stops is reported closed.
        harness.receive(
            JupyterMessage::new(
                CommOpen {
                    comm_id: CommId("explorer-2".to_string()),
                    target_name: DATA_EXPLORER_COMM_TARGET.to_string(),
                    data: json_object(&json!({ "title": "iris" })).unwrap(),
                    target_module: None,
                },
                None,
            ),
            cx,
        );
        harness.take_events();
        harness.session.update(cx, |session, cx| {
            session.handle_exit("R exited.".into(), cx)
        });
        assert!(harness.take_events().iter().any(|event| matches!(
            event,
            SessionEvent::DataExplorerClosed { comm_id } if comm_id == "explorer-2"
        )));
    }

    #[gpui::test]
    fn test_stop_asks_ark_to_shut_down(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let mut harness = Harness::new(cx);
        harness.finish_startup(cx);

        let stopped = harness.session.update(cx, |session, cx| session.stop(cx));
        let request = harness.next_request();
        assert!(matches!(
            request.content,
            JupyterMessageContent::ShutdownRequest(ShutdownRequest { restart: false })
        ));
        cx.foreground_executor().block_test(stopped);
        harness.session.update(cx, |session, cx| {
            assert!(matches!(session.state(), SessionState::Exited(_)));
            assert!(session.execute("1".into(), cx).is_err());
        });
    }

    #[test]
    fn test_launch_directory() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("analysis.R");
        std::fs::write(&script, "1 + 1\n").unwrap();

        assert_eq!(launch_directory(temp.path()).unwrap(), temp.path());
        assert_eq!(launch_directory(&script).unwrap(), temp.path());
        let missing = temp.path().join("missing").join("project");
        let error = launch_directory(&missing).unwrap_err().to_string();
        assert!(error.contains("doesn't exist"), "{error}");
    }

    #[gpui::test]
    fn test_missing_project_folder_is_reported(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let r = RToolchainData {
            r_home: PathBuf::from("/opt/R/4.5.3/lib/R"),
            version: Some("4.5.3".to_string()),
            source: languages::r::RInstallationSource::Rig,
        };
        let session = cx.new(|cx| {
            RSession::new(
                Some(r),
                PathBuf::from("/definitely/not/a/real/project"),
                HashMap::default(),
                None,
                cx,
            )
        });
        session.update_in(cx, |session, window, cx| session.start(window, cx));
        session.read_with(cx, |session, _| {
            let SessionState::Unavailable(message) = session.state() else {
                panic!("expected Unavailable, got {:?}", session.state());
            };
            assert!(
                message.contains("/definitely/not/a/real/project"),
                "{message}"
            );
        });
    }

    #[gpui::test]
    fn test_unavailable_without_r(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        let session = cx.new(|cx| {
            RSession::new(
                None,
                PathBuf::from("/projects/analysis"),
                HashMap::default(),
                None,
                cx,
            )
        });
        session.update_in(cx, |session, window, cx| session.start(window, cx));
        session.update(cx, |session, cx| {
            assert!(matches!(session.state(), SessionState::Unavailable(_)));
            assert!(session.execute("1".into(), cx).is_err());
        });
    }
}
