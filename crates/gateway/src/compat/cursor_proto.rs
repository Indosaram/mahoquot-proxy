#[derive(Clone, PartialEq, prost::Message)]
pub struct AgentClientMessage {
    #[prost(oneof = "agent_client_message::Message", tags = "1, 2, 3, 4, 5, 7")]
    pub message: Option<agent_client_message::Message>,
}

pub mod agent_client_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "1")]
        RunRequest(Box<super::AgentRunRequest>),
        #[prost(message, tag = "2")]
        ExecClientMessage(super::ExecClientMessage),
        #[prost(message, tag = "3")]
        KvClientMessage(super::KvClientMessage),
        #[prost(message, tag = "4")]
        ConversationAction(super::ConversationAction),
        #[prost(message, tag = "5")]
        ExecClientControlMessage(super::ExecClientControlMessage),
        #[prost(message, tag = "7")]
        ClientHeartbeat(super::ClientHeartbeat),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct AgentRunRequest {
    #[prost(message, optional, tag = "1")]
    pub conversation_state: Option<ConversationStateStructure>,
    #[prost(message, optional, tag = "2")]
    pub action: Option<ConversationAction>,
    #[prost(message, optional, tag = "3")]
    pub model_details: Option<ModelDetails>,
    #[prost(message, optional, tag = "4")]
    pub mcp_tools: Option<McpTools>,
    #[prost(string, optional, tag = "5")]
    pub conversation_id: Option<String>,
    /// Live AgentRunRequest field 8. A system blob in
    /// `root_prompt_messages_json` makes the current server return "Error".
    #[prost(string, optional, tag = "8")]
    pub custom_system_prompt: Option<String>,
    #[prost(message, optional, tag = "9")]
    pub requested_model: Option<RequestedModel>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ConversationStateStructure {
    #[prost(bytes = "vec", repeated, tag = "1")]
    pub root_prompt_messages_json: Vec<Vec<u8>>,
    #[prost(bytes = "vec", repeated, tag = "8")]
    pub turns: Vec<Vec<u8>>,
    #[prost(string, repeated, tag = "9")]
    pub previous_workspace_uris: Vec<String>,
    #[prost(int32, optional, tag = "10")]
    pub mode: Option<i32>,
    #[prost(string, tag = "22")]
    pub client_name: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ConversationAction {
    #[prost(oneof = "conversation_action::Action", tags = "1, 3")]
    pub action: Option<conversation_action::Action>,
}

pub mod conversation_action {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Action {
        #[prost(message, tag = "1")]
        UserMessageAction(super::UserMessageAction),
        #[prost(message, tag = "3")]
        CancelAction(super::CancelAction),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct UserMessageAction {
    #[prost(message, optional, tag = "1")]
    pub user_message: Option<UserMessage>,
}

/// Live cursor-agent 2026.10.22 `UserMessage.content` text case.
/// Field 1 is a message, not a string. A string here is parsed as a nested
/// message and the byte `o` (0x6F) becomes `illegal tag: field no 13 wire type 7`.
#[derive(Clone, PartialEq, prost::Message)]
pub struct UserMessageText {
    #[prost(string, tag = "1")]
    pub text: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct UserMessageContent {
    #[prost(message, optional, tag = "1")]
    pub text: Option<UserMessageText>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct UserMessage {
    #[prost(message, optional, tag = "1")]
    pub content: Option<UserMessageContent>,
    #[prost(string, tag = "2")]
    pub message_id: String,
    #[prost(int32, tag = "4")]
    pub mode: i32,
    #[prost(string, tag = "17")]
    pub correlation_id: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ModelDetails {
    #[prost(string, tag = "1")]
    pub model_id: String,
    #[prost(string, tag = "3")]
    pub display_model_id: String,
    #[prost(string, tag = "4")]
    pub display_name: String,
    #[prost(string, tag = "5")]
    pub display_name_short: String,
    #[prost(bool, optional, tag = "7")]
    pub max_mode: Option<bool>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RequestedModel {
    #[prost(string, tag = "1")]
    pub model_id: String,
    #[prost(bool, tag = "2")]
    pub max_mode: bool,
    #[prost(message, repeated, tag = "3")]
    pub parameters: Vec<RequestedModelParameter>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RequestedModelParameter {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, tag = "2")]
    pub value: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct McpTools {
    #[prost(message, repeated, tag = "1")]
    pub mcp_tools: Vec<McpToolDefinition>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct McpToolDefinition {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub description: String,
    /// Live cursor-agent field 3 is a protobuf Value, not raw JSON bytes.
    /// Raw JSON here is parsed as a message and `o` in "object" becomes
    /// `illegal tag: field no 13 wire type 7`.
    #[prost(message, optional, tag = "3")]
    pub input_schema: Option<prost_types::Value>,
    #[prost(string, tag = "4")]
    pub provider_identifier: String,
    #[prost(string, tag = "5")]
    pub tool_name: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct CancelAction {}
#[derive(Clone, PartialEq, prost::Message)]
pub struct ClientHeartbeat {}
#[derive(Clone, PartialEq, prost::Message)]
pub struct ExecClientMessage {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(string, tag = "15")]
    pub exec_id: String,
    #[prost(
        oneof = "exec_client_message::Message",
        tags = "2, 3, 4, 5, 7, 8, 9, 10, 14"
    )]
    pub message: Option<exec_client_message::Message>,
}
pub mod exec_client_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "2")]
        ShellResult(super::ShellResult),
        #[prost(message, tag = "3")]
        WriteResult(super::WriteResult),
        #[prost(message, tag = "4")]
        DeleteResult(super::DeleteResult),
        #[prost(message, tag = "5")]
        GrepResult(super::GrepResult),
        #[prost(message, tag = "7")]
        ReadResult(super::ReadResult),
        #[prost(message, tag = "8")]
        LsResult(super::LsResult),
        #[prost(message, tag = "9")]
        DiagnosticsResult(super::DiagnosticsResult),
        #[prost(message, tag = "10")]
        RequestContextResult(super::RequestContextResult),
        #[prost(message, tag = "14")]
        ShellStream(super::ShellStream),
        #[prost(message, tag = "17")]
        ListMcpResourcesExecResult(super::ListMcpResourcesExecResult),
        #[prost(message, tag = "18")]
        ReadMcpResourceExecResult(super::ReadMcpResourceExecResult),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct KvClientMessage {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(oneof = "kv_client_message::Message", tags = "2, 3")]
    pub message: Option<kv_client_message::Message>,
}
pub mod kv_client_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "2")]
        GetBlobResult(super::GetBlobResult),
        #[prost(message, tag = "3")]
        SetBlobResult(super::SetBlobResult),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct AgentServerMessage {
    #[prost(oneof = "agent_server_message::Message", tags = "1, 2, 3, 4")]
    pub message: Option<agent_server_message::Message>,
}

pub mod agent_server_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "1")]
        InteractionUpdate(super::InteractionUpdate),
        #[prost(message, tag = "2")]
        ExecServerMessage(super::ExecServerMessage),
        #[prost(message, tag = "3")]
        ConversationCheckpointUpdate(super::ConversationStateStructure),
        #[prost(message, tag = "4")]
        KvServerMessage(super::KvServerMessage),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct InteractionUpdate {
    #[prost(oneof = "interaction_update::Message", tags = "1, 2, 3, 4, 7, 8, 14")]
    pub message: Option<interaction_update::Message>,
}

pub mod interaction_update {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "1")]
        TextDelta(super::TextDeltaUpdate),
        #[prost(message, tag = "2")]
        ToolCallStarted(super::ToolCallUpdate),
        #[prost(message, tag = "3")]
        ToolCallCompleted(super::ToolCallUpdate),
        #[prost(message, tag = "4")]
        ThinkingDelta(super::TextDeltaUpdate),
        #[prost(message, tag = "7")]
        PartialToolCall(super::PartialToolCallUpdate),
        #[prost(message, tag = "8")]
        TokenDelta(super::TokenDeltaUpdate),
        #[prost(message, tag = "14")]
        TurnEnded(super::TurnEndedUpdate),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TextDeltaUpdate {
    #[prost(string, tag = "1")]
    pub text: String,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct TokenDeltaUpdate {
    #[prost(int32, tag = "1")]
    pub tokens: i32,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct TurnEndedUpdate {
    #[prost(uint64, tag = "1")]
    pub input_tokens: u64,
    #[prost(uint64, tag = "2")]
    pub output_tokens: u64,
    #[prost(uint64, tag = "3")]
    pub cache_read_tokens: u64,
    #[prost(uint64, tag = "4")]
    pub cache_write_tokens: u64,
    #[prost(uint64, tag = "5")]
    pub reasoning_tokens: u64,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct ToolCallUpdate {
    #[prost(string, tag = "1")]
    pub call_id: String,
    #[prost(message, optional, tag = "2")]
    pub tool_call: Option<ToolCall>,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct PartialToolCallUpdate {
    #[prost(string, tag = "1")]
    pub call_id: String,
    #[prost(message, optional, tag = "2")]
    pub tool_call: Option<ToolCall>,
    #[prost(string, tag = "3")]
    pub args_text_delta: String,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct ToolCall {
    #[prost(message, optional, tag = "15")]
    pub mcp_tool_call: Option<McpToolCall>,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct McpToolCall {
    #[prost(message, optional, tag = "1")]
    pub args: Option<McpArgs>,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct McpArgs {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(map = "string, bytes", tag = "2")]
    pub args: std::collections::HashMap<String, Vec<u8>>,
    #[prost(string, tag = "3")]
    pub tool_call_id: String,
    #[prost(string, tag = "4")]
    pub provider_identifier: String,
    #[prost(string, tag = "5")]
    pub tool_name: String,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct ExecServerMessage {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(string, tag = "15")]
    pub exec_id: String,
    #[prost(
        oneof = "exec_server_message::Message",
        tags = "2, 3, 4, 5, 7, 8, 9, 10, 11, 14, 16, 20, 21, 22, 23"
    )]
    pub message: Option<exec_server_message::Message>,
}
pub mod exec_server_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "2")]
        ShellArgs(super::ShellArgs),
        #[prost(message, tag = "3")]
        WriteArgs(super::WriteArgs),
        #[prost(message, tag = "4")]
        DeleteArgs(super::DeleteArgs),
        #[prost(message, tag = "5")]
        GrepArgs(super::GrepArgs),
        #[prost(message, tag = "7")]
        ReadArgs(super::ReadArgs),
        #[prost(message, tag = "8")]
        LsArgs(super::LsArgs),
        #[prost(message, tag = "9")]
        DiagnosticsArgs(super::DiagnosticsArgs),
        #[prost(message, tag = "10")]
        RequestContextArgs(super::RequestContextArgs),
        #[prost(message, tag = "11")]
        McpArgs(super::ExecMcpArgs),
        #[prost(message, tag = "14")]
        ShellStreamArgs(super::ShellArgs),
        #[prost(message, tag = "16")]
        BackgroundShellSpawnArgs(super::DiagnosticsArgs),
        #[prost(message, tag = "20")]
        FetchArgs(super::DiagnosticsArgs),
        #[prost(message, tag = "21")]
        RecordScreenArgs(super::DiagnosticsArgs),
        #[prost(message, tag = "22")]
        ComputerUseArgs(super::DiagnosticsArgs),
        #[prost(message, tag = "23")]
        WriteShellStdinArgs(super::DiagnosticsArgs),
        #[prost(message, tag = "17")]
        ListMcpResourcesExecArgs(super::ListMcpResourcesExecArgs),
        #[prost(message, tag = "18")]
        ReadMcpResourceExecArgs(super::ReadMcpResourceExecArgs),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ListMcpResourcesExecArgs {
    #[prost(string, optional, tag = "1")]
    pub server: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ListMcpResourcesSuccess {
    #[prost(message, repeated, tag = "1")]
    pub resources: Vec<ListMcpResourcesResource>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ListMcpResourcesResource {
    #[prost(string, tag = "1")]
    pub uri: String,
    #[prost(string, tag = "2")]
    pub name: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ListMcpResourcesError {
    #[prost(string, tag = "1")]
    pub error: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ListMcpResourcesExecResult {
    #[prost(oneof = "list_mcp_resources_exec_result::Result", tags = "1, 2")]
    pub result: Option<list_mcp_resources_exec_result::Result>,
}

pub mod list_mcp_resources_exec_result {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "1")]
        Success(super::ListMcpResourcesSuccess),
        #[prost(message, tag = "2")]
        Error(super::ListMcpResourcesError),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ReadMcpResourceExecArgs {
    #[prost(string, optional, tag = "1")]
    pub uri: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ReadMcpResourceSuccess {
    #[prost(string, tag = "1")]
    pub uri: String,
    #[prost(string, tag = "2")]
    pub mime_type: String,
    #[prost(string, tag = "3")]
    pub text: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ReadMcpResourceError {
    #[prost(string, tag = "1")]
    pub error: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ReadMcpResourceExecResult {
    #[prost(oneof = "read_mcp_resource_exec_result::Result", tags = "1, 2, 3")]
    pub result: Option<read_mcp_resource_exec_result::Result>,
}

pub mod read_mcp_resource_exec_result {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "1")]
        Success(super::ReadMcpResourceSuccess),
        #[prost(message, tag = "2")]
        Error(super::ReadMcpResourceError),
        #[prost(message, tag = "3")]
        Rejected(super::ReadMcpResourceError),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ShellArgs {
    #[prost(string, tag = "1")]
    pub command: String,
    #[prost(string, tag = "2")]
    pub working_directory: String,
    #[prost(int32, tag = "3")]
    pub timeout: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ReadArgs {
    #[prost(string, tag = "1")]
    pub path: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WriteArgs {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, tag = "2")]
    pub file_text: String,
    #[prost(string, tag = "3")]
    pub tool_call_id: String,
    #[prost(bool, tag = "4")]
    pub return_file_content_after_write: bool,
    #[prost(bytes, tag = "5")]
    pub file_bytes: Vec<u8>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct DeleteArgs {
    #[prost(string, tag = "1")]
    pub path: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct GrepArgs {
    #[prost(string, tag = "1")]
    pub pattern: String,
    #[prost(string, optional, tag = "2")]
    pub path: Option<String>,
    #[prost(string, optional, tag = "3")]
    pub glob: Option<String>,
    #[prost(string, optional, tag = "4")]
    pub output_mode: Option<String>,
    #[prost(int32, optional, tag = "5")]
    pub context_before: Option<i32>,
    #[prost(int32, optional, tag = "6")]
    pub context_after: Option<i32>,
    #[prost(int32, optional, tag = "7")]
    pub context: Option<i32>,
    #[prost(bool, optional, tag = "8")]
    pub case_insensitive: Option<bool>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct LsArgs {
    #[prost(string, tag = "1")]
    pub path: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct DiagnosticsArgs {}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ExecMcpArgs {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(map = "string, bytes", tag = "2")]
    pub args: std::collections::HashMap<String, Vec<u8>>,
    #[prost(string, tag = "3")]
    pub tool_call_id: String,
    #[prost(string, tag = "4")]
    pub provider_identifier: String,
    #[prost(string, tag = "5")]
    pub tool_name: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ShellSuccess {
    #[prost(string, tag = "1")]
    pub command: String,
    #[prost(string, tag = "2")]
    pub working_directory: String,
    #[prost(int32, tag = "3")]
    pub exit_code: i32,
    #[prost(string, tag = "4")]
    pub signal: String,
    #[prost(string, tag = "5")]
    pub stdout: String,
    #[prost(string, tag = "6")]
    pub stderr: String,
    #[prost(int32, tag = "7")]
    pub execution_time: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ShellResult {
    #[prost(oneof = "shell_result::Result", tags = "1, 2")]
    pub result: Option<shell_result::Result>,
}
pub mod shell_result {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "1")]
        Success(super::ShellSuccess),
        #[prost(message, tag = "2")]
        Failure(super::ShellSuccess),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ReadSuccess {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, optional, tag = "2")]
    pub content: Option<String>,
    #[prost(int32, tag = "3")]
    pub total_lines: i32,
    #[prost(int64, tag = "4")]
    pub file_size: i64,
    #[prost(bool, tag = "6")]
    pub truncated: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ReadFailure {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, tag = "2")]
    pub error: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ReadResult {
    #[prost(oneof = "read_result::Result", tags = "1, 2")]
    pub result: Option<read_result::Result>,
}
pub mod read_result {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "1")]
        Success(super::ReadSuccess),
        #[prost(message, tag = "2")]
        Error(super::ReadFailure),
        #[prost(message, tag = "4")]
        FileNotFound(super::ReadFileNotFound),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ReadFileNotFound {
    #[prost(string, tag = "1")]
    pub path: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WriteSuccess {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(int32, tag = "2")]
    pub lines_created: i32,
    #[prost(int32, tag = "3")]
    pub file_size: i32,
    #[prost(string, optional, tag = "4")]
    pub file_content_after_write: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WriteResult {
    #[prost(oneof = "write_result::Result", tags = "1, 5")]
    pub result: Option<write_result::Result>,
}
pub mod write_result {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "1")]
        Success(super::WriteSuccess),
        #[prost(message, tag = "5")]
        Error(super::ReadFailure),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct DeleteSuccess {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, tag = "2")]
    pub deleted_file: String,
    #[prost(int64, tag = "3")]
    pub file_size: i64,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct DeleteResult {
    #[prost(oneof = "delete_result::Result", tags = "1, 7")]
    pub result: Option<delete_result::Result>,
}
pub mod delete_result {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "1")]
        Success(super::DeleteSuccess),
        #[prost(message, tag = "2")]
        FileNotFound(super::ReadFileNotFound),
        #[prost(message, tag = "7")]
        Error(super::ReadFailure),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct LsResult {
    #[prost(oneof = "ls_result::Result", tags = "1, 2")]
    pub result: Option<ls_result::Result>,
}
pub mod ls_result {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "1")]
        Success(super::LsSuccess),
        #[prost(message, tag = "2")]
        Error(super::ReadFailure),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct LsSuccess {
    #[prost(message, optional, tag = "1")]
    pub directory_tree_root: Option<LsDirectoryTreeNode>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct LsDirectoryTreeNode {
    #[prost(string, tag = "1")]
    pub abs_path: String,
    #[prost(int32, tag = "6")]
    pub num_files: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct GrepResult {
    #[prost(oneof = "grep_result::Result", tags = "1, 2")]
    pub result: Option<grep_result::Result>,
}
pub mod grep_result {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "1")]
        Success(super::GrepSuccess),
        #[prost(message, tag = "2")]
        Error(super::GrepFailure),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct GrepSuccess {
    #[prost(string, tag = "1")]
    pub pattern: String,
    #[prost(string, tag = "2")]
    pub path: String,
    #[prost(string, tag = "3")]
    pub output_mode: String,
    #[prost(map = "string, message", tag = "4")]
    pub workspace_results: std::collections::HashMap<String, GrepUnionResult>,

    #[prost(message, optional, tag = "5")]
    pub active_editor_result: Option<GrepUnionResult>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct GrepUnionResult {
    #[prost(oneof = "grep_union_result::Result", tags = "1, 2, 3")]
    pub result: Option<grep_union_result::Result>,
}

pub mod grep_union_result {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "1")]
        Count(super::GrepCountResult),
        #[prost(message, tag = "2")]
        Files(super::GrepFilesResult),
        #[prost(message, tag = "3")]
        Content(super::GrepContentResult),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct GrepCountResult {
    #[prost(message, repeated, tag = "1")]
    pub counts: Vec<GrepFileCount>,
    #[prost(int32, tag = "2")]
    pub total_files: i32,
    #[prost(int32, tag = "3")]
    pub total_matches: i32,
    #[prost(bool, tag = "4")]
    pub client_truncated: bool,
    #[prost(bool, tag = "5")]
    pub ripgrep_truncated: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct GrepFileCount {
    #[prost(string, tag = "1")]
    pub file: String,
    #[prost(int32, tag = "2")]
    pub count: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct GrepFilesResult {
    #[prost(string, repeated, tag = "1")]
    pub files: Vec<String>,
    #[prost(int32, tag = "2")]
    pub total_files: i32,
    #[prost(bool, tag = "3")]
    pub client_truncated: bool,
    #[prost(bool, tag = "4")]
    pub ripgrep_truncated: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct GrepContentResult {
    #[prost(message, repeated, tag = "1")]
    pub matches: Vec<GrepFileMatch>,
    #[prost(int32, tag = "2")]
    pub total_lines: i32,
    #[prost(int32, tag = "3")]
    pub total_matched_lines: i32,
    #[prost(bool, tag = "4")]
    pub client_truncated: bool,
    #[prost(bool, tag = "5")]
    pub ripgrep_truncated: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct GrepFileMatch {
    #[prost(string, tag = "1")]
    pub file: String,
    #[prost(message, repeated, tag = "2")]
    pub matches: Vec<GrepContentMatch>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct GrepContentMatch {
    #[prost(int32, tag = "1")]
    pub line_number: i32,
    #[prost(string, tag = "2")]
    pub content: String,
    #[prost(bool, tag = "3")]
    pub content_truncated: bool,
    #[prost(bool, tag = "4")]
    pub is_context_line: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct GrepFailure {
    #[prost(string, tag = "1")]
    pub error: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct DiagnosticsResult {}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ExecClientControlMessage {
    #[prost(oneof = "exec_client_control_message::Message", tags = "1, 2, 3")]
    pub message: Option<exec_client_control_message::Message>,
}
pub mod exec_client_control_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "1")]
        StreamClose(super::ExecClientStreamClose),
        #[prost(message, tag = "2")]
        Throw(super::ExecClientThrow),
        #[prost(message, tag = "3")]
        Heartbeat(super::ShellStreamStart),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ExecClientStreamClose {
    #[prost(uint32, tag = "1")]
    pub id: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ExecClientThrow {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(string, tag = "2")]
    pub error: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ShellStream {
    #[prost(oneof = "shell_stream::Event", tags = "1, 2, 3, 4, 5, 6, 7")]
    pub event: Option<shell_stream::Event>,
}
pub mod shell_stream {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Event {
        #[prost(message, tag = "1")]
        Stdout(super::ShellStreamStdout),
        #[prost(message, tag = "2")]
        Stderr(super::ShellStreamStderr),
        #[prost(message, tag = "3")]
        Exit(super::ShellStreamExit),
        #[prost(message, tag = "4")]
        Start(super::ShellStreamStart),
        #[prost(message, tag = "5")]
        Rejected(super::ReadFailure),
        #[prost(message, tag = "6")]
        PermissionDenied(super::ReadFailure),
        #[prost(message, tag = "7")]
        Backgrounded(super::ShellStreamStart),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ShellStreamStdout {
    #[prost(string, tag = "1")]
    pub data: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ShellStreamStderr {
    #[prost(string, tag = "1")]
    pub data: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ShellStreamExit {
    #[prost(uint32, tag = "1")]
    pub code: u32,
    #[prost(string, tag = "2")]
    pub cwd: String,
    #[prost(bool, tag = "4")]
    pub aborted: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ShellStreamStart {}

#[derive(Clone, PartialEq, prost::Message)]
pub struct KvServerMessage {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(oneof = "kv_server_message::Message", tags = "2, 3")]
    pub message: Option<kv_server_message::Message>,
}
pub mod kv_server_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Message {
        #[prost(message, tag = "2")]
        GetBlobArgs(super::GetBlobArgs),
        #[prost(message, tag = "3")]
        SetBlobArgs(super::SetBlobArgs),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct GetBlobArgs {
    #[prost(bytes = "vec", tag = "1")]
    pub blob_id: Vec<u8>,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct GetBlobResult {
    #[prost(bytes = "vec", optional, tag = "1")]
    pub blob_data: Option<Vec<u8>>,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct SetBlobArgs {
    #[prost(bytes = "vec", tag = "1")]
    pub blob_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub blob_data: Vec<u8>,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct SetBlobResult {
    #[prost(message, optional, tag = "1")]
    pub error: Option<ProtoError>,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct ProtoError {}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RequestContextArgs {
    #[prost(string, optional, tag = "2")]
    pub notes_session_id: Option<String>,
    #[prost(string, optional, tag = "3")]
    pub workspace_id: Option<String>,
    #[prost(bool, optional, tag = "7")]
    pub use_cached: Option<bool>,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct RequestContextResult {
    #[prost(oneof = "request_context_result::Result", tags = "1, 2, 3")]
    pub result: Option<request_context_result::Result>,
}
pub mod request_context_result {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Result {
        #[prost(message, tag = "1")]
        Success(super::RequestContextSuccess),
        #[prost(message, tag = "2")]
        Error(super::RequestContextError),
        #[prost(message, tag = "3")]
        Rejected(super::RequestContextRejected),
    }
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct RequestContextSuccess {
    #[prost(message, optional, tag = "1")]
    pub request_context: Option<RequestContext>,
    #[prost(bool, optional, tag = "2")]
    pub served_from_disk_cache: Option<bool>,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct RequestContextError {
    #[prost(string, tag = "1")]
    pub error: String,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct RequestContextRejected {
    #[prost(string, tag = "1")]
    pub reason: String,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct RequestContext {
    #[prost(message, optional, tag = "4")]
    pub env: Option<RequestContextEnv>,
    #[prost(message, repeated, tag = "7")]
    pub tools: Vec<McpToolDefinition>,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct RequestContextEnv {
    #[prost(string, tag = "1")]
    pub os_version: String,
    #[prost(string, repeated, tag = "2")]
    pub workspace_paths: Vec<String>,
    #[prost(string, tag = "3")]
    pub shell: String,
    #[prost(bool, tag = "5")]
    pub sandbox_enabled: bool,
    #[prost(string, tag = "10")]
    pub time_zone: String,
}
