//! A session connected to a kernel that a test plays by hand: the test reads
//! what the session sends and delivers Ark's messages itself.

use crate::comms::{RPC_REPLY_MSG_TYPE, RPC_REQUEST_MSG_TYPE};
use crate::{KernelLauncher, KernelSession};
use collections::HashMap;
use futures::channel::mpsc;
use gpui::{AppContext as _, Entity, VisualTestContext};
use jupyter_protocol::{
    CommId, CommMsg, JupyterMessage, JupyterMessageContent, KernelInfoReply, UnknownMessage,
};
use languages::r::{RInstallationSource, RToolchainData};
use std::path::PathBuf;

pub struct FakeKernel {
    pub session: Entity<KernelSession>,
    /// What the session sent on the shell and control channels, oldest first.
    pub requests: mpsc::Receiver<JupyterMessage>,
    pub stdin: mpsc::Receiver<JupyterMessage>,
    kernel_info_request: Option<JupyterMessage>,
}

/// The comms a session opens when it connects.
pub struct StartupComms {
    pub ui: CommId,
    pub lsp: CommId,
    pub variables: CommId,
    pub help: CommId,
}

/// A session for a made-up R installation, not yet connected to anything.
pub fn fake_session(
    project_root: impl Into<PathBuf>,
    cx: &mut VisualTestContext,
) -> Entity<KernelSession> {
    let r = RToolchainData {
        r_home: PathBuf::from("/opt/R/4.5.3/lib/R"),
        version: Some("4.5.3".to_string()),
        source: RInstallationSource::Rig,
    };
    let project_root = project_root.into();
    cx.new(|cx| {
        KernelSession::new(
            KernelLauncher::ark(Some(r), None),
            project_root,
            HashMap::default(),
            cx,
        )
    })
}

impl FakeKernel {
    /// Connects a [`fake_session`] for `project_root`. Its startup requests
    /// wait in `requests`; see [`Self::finish_startup`].
    pub fn start(project_root: impl Into<PathBuf>, cx: &mut VisualTestContext) -> Self {
        Self::connect(fake_session(project_root, cx), cx)
    }

    pub fn connect(session: Entity<KernelSession>, cx: &mut VisualTestContext) -> Self {
        let (requests_tx, requests) = mpsc::channel(100);
        let (stdin_tx, stdin) = mpsc::channel(100);
        session.update(cx, |session, cx| {
            session.connect_for_test(requests_tx, stdin_tx, cx)
        });
        Self {
            session,
            requests,
            stdin,
            kernel_info_request: None,
        }
    }

    /// Takes the requests a session sends as it connects off `requests` and
    /// returns the comms it opened. The session is still starting until
    /// [`Self::answer_kernel_info`].
    pub fn take_startup_requests(&mut self) -> StartupComms {
        let kernel_info = self.next_request();
        assert!(
            matches!(
                kernel_info.content,
                JupyterMessageContent::KernelInfoRequest(_)
            ),
            "expected kernel_info_request, got {}",
            kernel_info.content.message_type()
        );
        self.kernel_info_request = Some(kernel_info);
        let ui = opened_comm(&self.next_request());
        let _frontend_ready = self.next_request();
        let lsp = opened_comm(&self.next_request());
        let variables = opened_comm(&self.next_request());
        let help = opened_comm(&self.next_request());
        StartupComms {
            ui,
            lsp,
            variables,
            help,
        }
    }

    /// Answers the kernel info request taken by
    /// [`Self::take_startup_requests`], after which the session is idle.
    pub fn answer_kernel_info(&mut self, cx: &mut VisualTestContext) {
        let request = self
            .kernel_info_request
            .take()
            .expect("the startup requests must be taken first");
        self.receive(JupyterMessage::new(kernel_info_reply(), Some(&request)), cx);
    }

    /// Takes the startup requests and answers kernel info, leaving the
    /// session idle with nothing in `requests`.
    pub fn finish_startup(&mut self, cx: &mut VisualTestContext) -> StartupComms {
        let comms = self.take_startup_requests();
        self.answer_kernel_info(cx);
        comms
    }

    pub fn next_request(&mut self) -> JupyterMessage {
        self.requests.try_recv().expect("expected a request to Ark")
    }

    pub fn next_stdin(&mut self) -> JupyterMessage {
        self.stdin.try_recv().expect("expected a message on stdin")
    }

    /// Every request sent and not yet taken, oldest first.
    pub fn take_requests(&mut self) -> Vec<JupyterMessage> {
        std::iter::from_fn(|| self.requests.try_recv().ok()).collect()
    }

    /// Hands the session a message from the kernel, without running what
    /// that sets off.
    pub fn deliver(&self, message: JupyterMessage, cx: &mut VisualTestContext) {
        self.session.update_in(cx, |session, window, cx| {
            session.receive_for_test(&message, window, cx)
        });
    }

    /// Hands the session a message from the kernel and runs everything it
    /// sets off.
    pub fn receive(&self, message: JupyterMessage, cx: &mut VisualTestContext) {
        self.deliver(message, cx);
        cx.run_until_parked();
    }

    /// The kernel sending `data` on a comm, as for an event.
    pub fn send_comm_message(
        &self,
        comm_id: &CommId,
        data: serde_json::Value,
        cx: &mut VisualTestContext,
    ) {
        self.receive(comm_message(comm_id, data), cx);
    }

    /// The kernel answering `request`, a comm message the session sent, with
    /// `data` on the same comm.
    pub fn reply_to_comm_request(
        &self,
        request: &JupyterMessage,
        data: serde_json::Value,
        cx: &mut VisualTestContext,
    ) {
        let JupyterMessageContent::CommMsg(comm) = &request.content else {
            panic!(
                "expected a comm request, got {}",
                request.content.message_type()
            );
        };
        let serde_json::Value::Object(data) = data else {
            panic!("reply data must be an object");
        };
        let reply = JupyterMessage::new(
            CommMsg {
                comm_id: comm.comm_id.clone(),
                data,
            },
            Some(request),
        );
        self.receive(reply, cx);
    }
}

fn opened_comm(message: &JupyterMessage) -> CommId {
    match &message.content {
        JupyterMessageContent::CommOpen(open) => open.comm_id.clone(),
        other => panic!("expected comm_open, got {}", other.message_type()),
    }
}

pub fn kernel_info_reply() -> KernelInfoReply {
    serde_json::from_value(serde_json::json!({
        "status": "ok",
        "protocol_version": "5.3",
        "implementation": "ark",
        "implementation_version": "0.1.252",
        "language_info": { "name": "R", "version": "4.5.3" },
        "banner": "",
        "help_links": [],
        "debugger": false
    }))
    .expect("a valid kernel_info_reply")
}

/// A message from the kernel on a comm.
pub fn comm_message(comm_id: &CommId, data: serde_json::Value) -> JupyterMessage {
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

/// A request from Ark to the frontend, which arrives on stdin.
pub fn rpc_request(content: serde_json::Value) -> JupyterMessage {
    JupyterMessage::new(
        UnknownMessage {
            msg_type: RPC_REQUEST_MSG_TYPE.to_string(),
            content,
        },
        None,
    )
}

/// The content of `message`, which must be the frontend's reply to `request`.
pub fn rpc_reply_content(message: &JupyterMessage, request: &JupyterMessage) -> serde_json::Value {
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
