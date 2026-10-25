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
pub const VARIABLES_COMM_TARGET: &str = "positron.variables";
pub const DATA_EXPLORER_COMM_TARGET: &str = "positron.dataExplorer";

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
    /// The size new plots should be drawn at, so they arrive ready to show.
    #[serde(rename = "did_change_plots_render_settings")]
    DidChangePlotsRenderSettings { settings: PlotRenderSettings },
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
    let params = match message.remove("params") {
        None | Some(Value::Null) => None,
        Some(params) => Some(params),
    };
    let without_params = normalized.clone();
    let Some(params) = params else {
        return serde_json::from_value(Value::Object(without_params));
    };
    let params_are_empty = params.as_object().is_some_and(|params| params.is_empty());
    normalized.insert("params".to_string(), params);
    match serde_json::from_value(Value::Object(normalized)) {
        // A method without parameters rejects even empty ones.
        Err(_) if params_are_empty => serde_json::from_value(Value::Object(without_params)),
        result => result,
    }
}

// Variables comm (`positron.variables`), from Ark 0.1.252's `variables_comm.rs`.

/// The kind of value a variable holds. Kinds added by newer Ark versions read as `Other`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum VariableKind {
    #[serde(rename = "boolean")]
    Boolean,
    #[serde(rename = "bytes")]
    Bytes,
    #[serde(rename = "class")]
    Class,
    #[serde(rename = "collection")]
    Collection,
    #[serde(rename = "empty")]
    Empty,
    #[serde(rename = "function")]
    Function,
    #[serde(rename = "map")]
    Map,
    #[serde(rename = "number")]
    Number,
    #[serde(rename = "string")]
    String,
    #[serde(rename = "table")]
    Table,
    #[serde(rename = "lazy")]
    Lazy,
    #[serde(rename = "connection")]
    Connection,
    #[serde(rename = "other", other)]
    Other,
}

/// A variable in the R session, or a child of one.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Variable {
    /// Identifies the variable among its siblings; a path of these reaches nested values.
    pub access_key: String,
    pub display_name: String,
    /// Possibly truncated, see `is_truncated`.
    pub display_value: String,
    pub display_type: String,
    pub type_info: String,
    /// Bytes.
    pub size: i64,
    pub kind: VariableKind,
    /// Number of elements, for collections.
    pub length: i64,
    pub has_children: bool,
    /// Whether `view` can open it in the data explorer.
    pub has_viewer: bool,
    pub is_truncated: bool,
    /// Milliseconds since the epoch, or 0 if unknown.
    #[serde(default)]
    pub updated_time: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct VariableList {
    pub variables: Vec<Variable>,
    /// Can exceed `variables.len()` when the list is truncated.
    pub length: i64,
    pub version: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct InspectedVariable {
    pub children: Vec<Variable>,
    /// Can exceed `children.len()` when the list is truncated.
    pub length: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FormattedVariable {
    pub content: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct VariablesUpdate {
    pub assigned: Vec<Variable>,
    pub unevaluated: Vec<Variable>,
    /// Access keys of removed variables.
    pub removed: Vec<String>,
    pub version: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct VariablesRefresh {
    pub variables: Vec<Variable>,
    pub length: i64,
    pub version: i64,
}

/// Events the backend sends on the variables comm.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "method", content = "params")]
pub enum VariablesEvent {
    #[serde(rename = "update")]
    Update(VariablesUpdate),
    #[serde(rename = "refresh")]
    Refresh(VariablesRefresh),
}

// Data explorer comm (`positron.dataExplorer`), from Ark 0.1.252's `data_explorer_comm.rs`.
// Only the fields RezStudio uses are listed; others in Ark's messages are ignored.

/// Data sent with the `comm_open` Ark makes for `View()`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DataExplorerOpenData {
    pub title: String,
    #[serde(default)]
    pub inline_only: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TableShape {
    pub num_rows: i64,
    pub num_columns: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BackendState {
    pub display_name: String,
    /// With row and column filters applied.
    pub table_shape: TableShape,
    pub table_unfiltered_shape: TableShape,
    /// Whether rows have names; otherwise they're labeled by position.
    pub has_row_labels: bool,
    pub sort_keys: Vec<ColumnSortKey>,
    pub connected: Option<bool>,
    pub error_message: Option<String>,
}

/// Display types newer Ark versions add read as `Unknown`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ColumnDisplayType {
    #[serde(rename = "boolean")]
    Boolean,
    #[serde(rename = "string")]
    String,
    #[serde(rename = "date")]
    Date,
    #[serde(rename = "datetime")]
    Datetime,
    #[serde(rename = "time")]
    Time,
    #[serde(rename = "interval")]
    Interval,
    #[serde(rename = "object")]
    Object,
    #[serde(rename = "array")]
    Array,
    #[serde(rename = "struct")]
    Struct,
    #[serde(rename = "floating")]
    Floating,
    #[serde(rename = "integer")]
    Integer,
    #[serde(rename = "decimal")]
    Decimal,
    #[serde(rename = "unknown", other)]
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ColumnSchema {
    pub column_name: String,
    /// From R's `label` attribute, as set by haven or labelled.
    pub column_label: Option<String>,
    /// Position in the table without column filters.
    pub column_index: i64,
    /// R's own type name, such as `dbl` or `fct`.
    pub type_name: String,
    pub type_display: ColumnDisplayType,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TableSchema {
    pub columns: Vec<ColumnSchema>,
}

/// A formatted cell, or a code for a value with no text form.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ColumnValue {
    SpecialValueCode(i64),
    FormattedValue(String),
}

impl ColumnValue {
    /// The text to show, and whether it stands for a missing or special value.
    pub fn display(&self) -> (&str, bool) {
        match self {
            ColumnValue::FormattedValue(text) => (text, false),
            ColumnValue::SpecialValueCode(code) => (
                match code {
                    0 => "NULL",
                    1 => "NA",
                    2 => "NaN",
                    10 => "Inf",
                    11 => "-Inf",
                    _ => "?",
                },
                true,
            ),
        }
    }
}

/// Values for each requested column, in request order.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TableData {
    pub columns: Vec<Vec<ColumnValue>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TableRowLabels {
    pub row_labels: Vec<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FormatOptions {
    pub large_num_digits: i64,
    pub small_num_digits: i64,
    pub max_integral_digits: i64,
    pub max_value_length: i64,
    pub thousands_sep: Option<String>,
}

impl Default for FormatOptions {
    /// Positron's defaults.
    fn default() -> Self {
        Self {
            large_num_digits: 2,
            small_num_digits: 4,
            max_integral_digits: 7,
            max_value_length: 1000,
            thousands_sep: None,
        }
    }
}

/// Inclusive row range.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DataSelectionRange {
    pub first_index: i64,
    pub last_index: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DataSelectionIndices {
    pub indices: Vec<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum ArraySelection {
    SelectRange(DataSelectionRange),
    SelectIndices(DataSelectionIndices),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ColumnSelection {
    pub column_index: i64,
    pub spec: ArraySelection,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ColumnSortKey {
    pub column_index: i64,
    pub ascending: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ColumnProfileType {
    #[serde(rename = "null_count")]
    NullCount,
    #[serde(rename = "summary_stats")]
    SummaryStats,
    #[serde(rename = "small_frequency_table")]
    SmallFrequencyTable,
    #[serde(rename = "large_frequency_table")]
    LargeFrequencyTable,
    #[serde(rename = "small_histogram")]
    SmallHistogram,
    #[serde(rename = "large_histogram")]
    LargeHistogram,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ColumnProfileSpec {
    pub profile_type: ColumnProfileType,
    pub params: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ColumnProfileRequest {
    pub column_index: i64,
    pub profiles: Vec<ColumnProfileSpec>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ColumnProfileResult {
    pub null_count: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ReturnColumnProfiles {
    /// The `callback_id` of the `get_column_profiles` request this answers.
    pub callback_id: String,
    /// In the order of the request's `profiles`.
    pub profiles: Vec<ColumnProfileResult>,
    pub error_message: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct GetSchemaParams {
    pub column_indices: Vec<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct GetDataValuesParams {
    pub columns: Vec<ColumnSelection>,
    pub format_options: FormatOptions,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct GetRowLabelsParams {
    pub selection: ArraySelection,
    pub format_options: FormatOptions,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SetSortColumnsParams {
    pub sort_keys: Vec<ColumnSortKey>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct GetColumnProfilesParams {
    pub callback_id: String,
    pub profiles: Vec<ColumnProfileRequest>,
    pub format_options: FormatOptions,
}

/// Events the backend sends on a data explorer comm.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "method", content = "params")]
pub enum DataExplorerEvent {
    /// The columns changed; refetch the schema and data.
    #[serde(rename = "schema_update")]
    SchemaUpdate,
    /// The values changed; refetch the data.
    #[serde(rename = "data_update")]
    DataUpdate,
    #[serde(rename = "return_column_profiles")]
    ReturnColumnProfiles(ReturnColumnProfiles),
}

// UI comm requests that run R code: `call_method` runs `.ps.rpc.<method>`,
// and `evaluate_code` runs a line silently. Both reply with JSON.

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CallMethodParams {
    pub method: String,
    /// Passed to the R function in order.
    pub params: Vec<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EvaluateCodeParams {
    pub code: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EvalResult {
    pub result: Value,
    /// What the code printed while it ran.
    #[serde(default)]
    pub output: String,
}

// Packages, from Ark 0.1.252's `packages_pane.R`, which Positron's
// Packages pane calls through `call_method`.

/// One installed package, from `pkg_list`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct InstalledPackage {
    pub name: String,
    pub version: String,
    /// On the search path, as after `library()`; a package can be loaded
    /// without being attached.
    pub attached: bool,
    /// The DESCRIPTION's Description, on one line; empty when it has none.
    #[serde(default)]
    pub description: String,
    /// The first URL in the DESCRIPTION, usually the package's website.
    #[serde(default)]
    pub url: Option<String>,
}

/// The reply to `pkg_detail`, for an installed package.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PackageDetail {
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    /// The maintainer, or the authors when there's no maintainer.
    #[serde(default)]
    pub author: Option<String>,
    /// The primary license, without any "+ file LICENSE".
    #[serde(default)]
    pub license: Option<String>,
    /// Where it was installed from, such as `CRAN`.
    #[serde(default)]
    pub source_repository: Option<String>,
    #[serde(default)]
    pub published_date: Option<String>,
}

/// An installed package with a newer version in the session's repositories,
/// from `pkg_outdated`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OutdatedPackage {
    pub name: String,
    pub latest_version: String,
}

// Plot comm (`positron.plot`), from Ark 0.1.252's `plot_comm.rs`. Ark opens
// one per plot.

/// A size in logical pixels; `pixel_ratio` scales it to device pixels.
#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlotSize {
    pub width: i64,
    pub height: i64,
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PlotRenderFormat {
    Png,
    Jpeg,
    Svg,
    Pdf,
    Tiff,
}

impl PlotRenderFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Svg => "svg",
            Self::Pdf => "pdf",
            Self::Tiff => "tiff",
        }
    }
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PlotRenderSettings {
    pub size: PlotSize,
    pub pixel_ratio: f64,
    pub format: PlotRenderFormat,
}

/// A rendered plot.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PlotResult {
    /// The image, base64-encoded. Ark leaves out the `=` padding.
    pub data: String,
    pub mime_type: String,
    pub settings: Option<PlotRenderSettings>,
}

impl PlotResult {
    pub fn decode(&self) -> anyhow::Result<Vec<u8>> {
        use base64::Engine as _;
        let engine = base64::engine::GeneralPurpose::new(
            &base64::alphabet::STANDARD,
            base64::engine::GeneralPurposeConfig::new()
                .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
        );
        engine
            .decode(self.data.trim())
            .map_err(|error| anyhow::anyhow!("Ark sent a plot that isn't valid base64: {error}"))
    }
}

/// Data sent with the `comm_open` Ark makes for a new plot.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct PlotOpenData {
    /// The plot drawn at the last size the frontend asked for.
    #[serde(default)]
    pub pre_render: Option<PlotResult>,
}

/// Parameters for `render`.
#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RenderParams {
    /// Without a size, Ark uses the plot's intrinsic size, which most plots lack.
    pub size: Option<PlotSize>,
    pub pixel_ratio: f64,
    pub format: PlotRenderFormat,
}

/// The reply to `get_metadata`. Only the fields RezStudio uses are listed.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PlotMetadata {
    /// A human-readable name such as `ggplot2 1`.
    pub name: String,
    /// What drew it, such as `ggplot2` or `base`.
    pub kind: String,
    /// The code that drew it.
    #[serde(default)]
    pub code: String,
}

/// Events the backend sends on a plot comm. Each may carry a fresh render at
/// the last size the frontend asked for.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "method", content = "params")]
pub enum PlotEvent {
    /// More was drawn on the plot, as by `lines()` after `plot()`.
    #[serde(rename = "update")]
    Update {
        #[serde(default)]
        pre_render: Option<PlotResult>,
    },
    /// R asks for the plot to be brought to the front.
    #[serde(rename = "show")]
    Show {
        #[serde(default)]
        pre_render: Option<PlotResult>,
    },
}

/// The data of a comm RPC request: JSON-RPC's `id` must equal the Jupyter
/// message id, which is how Ark tells a request from an event. Pass `Null`
/// for methods without parameters, which Ark rejects if `params` is present.
pub fn rpc_request_data(id: &str, method: &str, params: Value) -> Value {
    if params.is_null() {
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method })
    } else {
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }
}

/// The result of a comm RPC reply, which is `{"method", "result"}` or `{"error"}`.
pub fn rpc_reply_result(data: Value) -> anyhow::Result<Value> {
    let Value::Object(mut data) = data else {
        anyhow::bail!("unexpected RPC reply: {data}");
    };
    if let Some(error) = data.remove("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| error.to_string());
        anyhow::bail!("{message}");
    }
    Ok(data.remove("result").unwrap_or(Value::Null))
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
    fn test_plot_messages() {
        // "hello" in base64 is "aGVsbG8=", which Ark sends without the padding.
        let pre_render = json!({
            "data": "aGVsbG8",
            "mime_type": "image/png",
            "settings": { "size": { "width": 640, "height": 400 }, "pixel_ratio": 1.0, "format": "png" }
        });
        let open: PlotOpenData =
            serde_json::from_value(json!({ "pre_render": pre_render })).unwrap();
        let result = open.pre_render.unwrap();
        assert_eq!(result.decode().unwrap(), b"hello");
        assert_eq!(
            result.settings.map(|settings| settings.size),
            Some(PlotSize {
                width: 640,
                height: 400
            })
        );
        let padded = PlotResult {
            data: "aGVsbG8=".into(),
            ..result
        };
        assert_eq!(padded.decode().unwrap(), b"hello");
        assert_eq!(
            serde_json::from_value::<PlotOpenData>(json!({})).unwrap(),
            PlotOpenData::default()
        );

        let event: PlotEvent =
            parse_method(json!({ "method": "update", "params": { "pre_render": null } })).unwrap();
        assert_eq!(event, PlotEvent::Update { pre_render: None });
        let event: PlotEvent = parse_method(json!({ "method": "show", "params": {} })).unwrap();
        assert_eq!(event, PlotEvent::Show { pre_render: None });

        assert_eq!(
            serde_json::to_value(UiBackendEvent::DidChangePlotsRenderSettings {
                settings: PlotRenderSettings {
                    size: PlotSize {
                        width: 500,
                        height: 300
                    },
                    pixel_ratio: 2.0,
                    format: PlotRenderFormat::Png,
                },
            })
            .unwrap(),
            json!({
                "method": "did_change_plots_render_settings",
                "params": { "settings": {
                    "size": { "width": 500, "height": 300 }, "pixel_ratio": 2.0, "format": "png"
                }}
            })
        );
        assert_eq!(
            serde_json::to_value(RenderParams {
                size: Some(PlotSize {
                    width: 800,
                    height: 600
                }),
                pixel_ratio: 2.0,
                format: PlotRenderFormat::Svg,
            })
            .unwrap(),
            json!({ "size": { "width": 800, "height": 600 }, "pixel_ratio": 2.0, "format": "svg" })
        );
    }

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
    fn test_parses_variables_and_data_explorer_messages() {
        let event: VariablesEvent = parse_method(json!({
            "method": "update",
            "params": {
                "assigned": [{
                    "access_key": "df", "display_name": "df", "display_value": "[3 rows x 2 columns] <data.frame>",
                    "display_type": "data.frame [3, 2]", "type_info": "data.frame", "size": 1024,
                    "kind": "table", "length": 2, "has_children": true, "has_viewer": true,
                    "is_truncated": false, "updated_time": 0
                }],
                "unevaluated": [],
                "removed": ["old"],
                "version": 3
            }
        }))
        .unwrap();
        let VariablesEvent::Update(update) = event else {
            panic!("expected an update");
        };
        assert_eq!(update.assigned[0].kind, VariableKind::Table);
        assert!(update.assigned[0].has_viewer);
        assert_eq!(update.removed, ["old"]);

        // Kinds and display types from newer Ark versions don't fail parsing.
        let kind: VariableKind = serde_json::from_value(json!("hologram")).unwrap();
        assert_eq!(kind, VariableKind::Other);
        let display: ColumnDisplayType = serde_json::from_value(json!("geometry")).unwrap();
        assert_eq!(display, ColumnDisplayType::Unknown);

        let data: TableData =
            serde_json::from_value(json!({ "columns": [["1.5", 1, "2.0"]] })).unwrap();
        assert_eq!(data.columns[0][1].display(), ("NA", true));
        assert_eq!(data.columns[0][0].display(), ("1.5", false));

        let event: DataExplorerEvent =
            parse_method(json!({ "method": "data_update", "params": {} })).unwrap();
        assert_eq!(event, DataExplorerEvent::DataUpdate);
        assert_eq!(
            serde_json::to_value(ArraySelection::SelectRange(DataSelectionRange {
                first_index: 0,
                last_index: 99
            }))
            .unwrap(),
            json!({ "first_index": 0, "last_index": 99 })
        );
    }

    #[test]
    fn test_rpc_envelopes() {
        assert_eq!(
            rpc_request_data("abc", "inspect", json!({ "path": ["df"] })),
            json!({ "jsonrpc": "2.0", "id": "abc", "method": "inspect", "params": { "path": ["df"] } })
        );
        assert_eq!(
            rpc_reply_result(
                json!({ "method": "inspect", "result": { "length": 0, "children": [] } })
            )
            .unwrap(),
            json!({ "length": 0, "children": [] })
        );
        let error =
            rpc_reply_result(json!({ "error": { "code": -32603, "message": "object not found" } }))
                .unwrap_err();
        assert_eq!(error.to_string(), "object not found");
        assert_eq!(
            rpc_reply_result(json!({ "method": "clear" })).unwrap(),
            Value::Null
        );
        assert_eq!(
            rpc_request_data("abc", "get_state", Value::Null),
            json!({ "jsonrpc": "2.0", "id": "abc", "method": "get_state" })
        );
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
