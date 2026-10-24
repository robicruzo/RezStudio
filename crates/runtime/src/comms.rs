//! Message types for the comms Ark opens with a frontend.
//!
//! The UI comm types are ported from Ark 0.1.252,
//! `crates/amalthea/src/comm/ui_comm.rs`, which is generated from Positron's
//! comm definitions and published by Posit under the MIT license:
//!
//! Copyright (C) 2024-2026 Posit Software, PBC.
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to
//! deal in the Software without restriction, including without limitation the
//! rights to use, copy, modify, merge, publish, distribute, sublicense, and/or
//! sell copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions: The above copyright
//! notice and this permission notice shall be included in all copies or
//! substantial portions of the Software.
//!
//! They must match the Ark version bundled with RezStudio (see `ARK_VERSION`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The Ark release whose comm protocol these types describe.
pub const ARK_VERSION: &str = "0.1.252";

pub const UI_COMM_TARGET: &str = "positron.ui";
pub const LSP_COMM_TARGET: &str = "positron.lsp";
pub const PLOT_COMM_TARGET: &str = "positron.plot";

/// Jupyter message types Ark uses on the stdin channel for requests it makes
/// of the frontend while R waits for the answer.
pub const RPC_REQUEST_MSG_TYPE: &str = "rpc_request";
pub const RPC_REPLY_MSG_TYPE: &str = "rpc_reply";

/// Data sent with the `comm_open` that starts the UI comm.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct UiCommOpenData {
    pub console_width: i32,
}

/// Data sent with the `comm_open` that starts Ark's language server.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ServerStartData {
    pub ip_address: String,
}

/// Ark's reply on the LSP comm once its server is listening.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "msg_type", content = "content")]
pub enum LspCommMessage {
    #[serde(rename = "server_started")]
    ServerStarted { port: u16 },
}

/// Events the frontend sends to the backend on the UI comm.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "method", content = "params")]
pub enum UiBackendEvent {
    #[serde(rename = "frontend_ready")]
    FrontendReady { start_type: String },
}

/// Events the backend sends to the frontend on the UI comm.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "method", content = "params")]
pub enum UiFrontendEvent {
    /// Whether a computation is running, as opposed to the kernel processing a message.
    #[serde(rename = "busy")]
    Busy(BusyParams),
    #[serde(rename = "clear_console")]
    ClearConsole,
    #[serde(rename = "open_editor")]
    OpenEditor(OpenEditorParams),
    #[serde(rename = "show_message")]
    ShowMessage(ShowMessageParams),
    #[serde(rename = "prompt_state")]
    PromptState(PromptStateParams),
    #[serde(rename = "working_directory")]
    WorkingDirectory(WorkingDirectoryParams),
    #[serde(rename = "open_workspace")]
    OpenWorkspace(OpenWorkspaceParams),
    #[serde(rename = "set_editor_selections")]
    SetEditorSelections(SetEditorSelectionsParams),
    #[serde(rename = "show_url")]
    ShowUrl(ShowUrlParams),
    #[serde(rename = "show_html_file")]
    ShowHtmlFile(ShowHtmlFileParams),
    #[serde(rename = "open_with_system")]
    OpenWithSystem(OpenWithSystemParams),
    #[serde(rename = "clear_webview_preloads")]
    ClearWebviewPreloads,
}

/// Requests the backend makes of the frontend, sent as `rpc_request` on stdin.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "method", content = "params")]
pub enum UiFrontendRequest {
    #[serde(rename = "new_document")]
    NewDocument(NewDocumentParams),
    #[serde(rename = "show_question")]
    ShowQuestion(ShowQuestionParams),
    #[serde(rename = "show_dialog")]
    ShowDialog(ShowDialogParams),
    #[serde(rename = "show_prompt")]
    ShowPrompt(ShowPromptParams),
    #[serde(rename = "ask_for_password")]
    AskForPassword(AskForPasswordParams),
    #[serde(rename = "debug_sleep")]
    DebugSleep(DebugSleepParams),
    #[serde(rename = "execute_command")]
    ExecuteCommand(ExecuteCommandParams),
    #[serde(rename = "evaluate_when_clause")]
    EvaluateWhenClause(EvaluateWhenClauseParams),
    #[serde(rename = "execute_code")]
    ExecuteCode(ExecuteCodeParams),
    #[serde(rename = "workspace_folder")]
    WorkspaceFolder,
    #[serde(rename = "modify_editor_selections")]
    ModifyEditorSelections(ModifyEditorSelectionsParams),
    #[serde(rename = "last_active_editor_context")]
    LastActiveEditorContext,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BusyParams {
    pub busy: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct OpenEditorParams {
    pub file: String,
    pub line: i64,
    pub column: i64,
    pub kind: OpenEditorKind,
    pub pinned: Option<bool>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub enum OpenEditorKind {
    #[serde(rename = "path")]
    Path,
    #[serde(rename = "uri")]
    Uri,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ShowMessageParams {
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PromptStateParams {
    pub input_prompt: String,
    pub continuation_prompt: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WorkingDirectoryParams {
    pub directory: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct OpenWorkspaceParams {
    pub path: String,
    pub new_window: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SetEditorSelectionsParams {
    pub selections: Vec<Range>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ShowUrlParams {
    pub url: String,
    pub source: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ShowHtmlFileParams {
    pub path: String,
    pub title: String,
    pub destination: String,
    pub height: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct OpenWithSystemParams {
    pub path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct NewDocumentParams {
    pub contents: String,
    pub language_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ShowQuestionParams {
    pub title: String,
    pub message: String,
    pub ok_button_title: String,
    pub cancel_button_title: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ShowDialogParams {
    pub title: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ShowPromptParams {
    pub title: String,
    pub message: String,
    pub default: String,
    pub timeout: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AskForPasswordParams {
    pub prompt: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DebugSleepParams {
    pub ms: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ExecuteCommandParams {
    pub command: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EvaluateWhenClauseParams {
    pub when_clause: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ExecuteCodeParams {
    pub language_id: String,
    pub code: String,
    pub focus: bool,
    pub allow_incomplete: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModifyEditorSelectionsParams {
    pub selections: Vec<Range>,
    pub values: Vec<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct Position {
    pub character: i64,
    pub line: i64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

/// Parses a `{"method": ..., "params": ...}` message. Ark omits `params` for
/// methods without parameters; an empty or null `params`, and any other
/// envelope fields such as JSON-RPC's `id`, are tolerated as well.
pub fn parse_method<T: serde::de::DeserializeOwned>(message: Value) -> serde_json::Result<T> {
    let Value::Object(mut message) = message else {
        return serde_json::from_value(message);
    };
    let mut normalized = serde_json::Map::new();
    if let Some(method) = message.remove("method") {
        normalized.insert("method".to_string(), method);
    }
    match message.remove("params") {
        None | Some(Value::Null) => {}
        Some(Value::Object(params)) if params.is_empty() => {}
        Some(params) => {
            normalized.insert("params".to_string(), params);
        }
    }
    serde_json::from_value(Value::Object(normalized))
}

/// The body of an `rpc_reply`.
pub fn rpc_result(result: Value) -> Value {
    serde_json::json!({ "result": result })
}

/// The body of an `rpc_reply` for a request the frontend cannot fulfil, which
/// R surfaces as an error instead of waiting forever.
pub fn rpc_error(message: impl Into<String>) -> Value {
    const INTERNAL_ERROR: i64 = -32603;
    serde_json::json!({ "error": { "code": INTERNAL_ERROR, "message": message.into() } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parses_frontend_events() {
        let event: UiFrontendEvent = parse_method(json!({
            "method": "prompt_state",
            "params": { "input_prompt": "Browse[1]> ", "continuation_prompt": "+ " }
        }))
        .unwrap();
        assert_eq!(
            event,
            UiFrontendEvent::PromptState(PromptStateParams {
                input_prompt: "Browse[1]> ".into(),
                continuation_prompt: "+ ".into(),
            })
        );

        for message in [
            json!({ "method": "clear_console" }),
            json!({ "method": "clear_console", "params": {} }),
            json!({ "method": "clear_console", "params": null }),
        ] {
            let event: UiFrontendEvent = parse_method(message).unwrap();
            assert_eq!(event, UiFrontendEvent::ClearConsole);
        }
    }

    #[test]
    fn test_parses_frontend_requests() {
        let request: UiFrontendRequest = parse_method(json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "show_question",
            "params": {
                "title": "Restart",
                "message": "Restart R?",
                "ok_button_title": "Yes",
                "cancel_button_title": "No"
            }
        }))
        .unwrap();
        assert!(matches!(request, UiFrontendRequest::ShowQuestion(_)));

        let request: UiFrontendRequest =
            parse_method(json!({ "method": "workspace_folder" })).unwrap();
        assert_eq!(request, UiFrontendRequest::WorkspaceFolder);
    }

    #[test]
    fn test_wire_formats() {
        assert_eq!(
            serde_json::to_value(UiBackendEvent::FrontendReady {
                start_type: "new".into()
            })
            .unwrap(),
            json!({ "method": "frontend_ready", "params": { "start_type": "new" } })
        );
        let started: LspCommMessage = serde_json::from_value(json!({
            "msg_type": "server_started",
            "content": { "port": 51234 }
        }))
        .unwrap();
        assert_eq!(started, LspCommMessage::ServerStarted { port: 51234 });
        assert_eq!(
            rpc_error("nope"),
            json!({ "error": { "code": -32603, "message": "nope" } })
        );
    }
}
