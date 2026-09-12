//! Devin Connect protobuf types, hand-mapped from the pinned schema.
//!
//! Field numbers match the upstream `devin.proto` snapshot at
//! `dsh-plugin-devin-bridge@ced035dd2f43b2dbf37a25c8a47b8ae1c1cc47f4`
//! (`src/proto/devin.proto`), which itself mirrors
//! `exa.api_server_pb` on `server.codeium.com`. All messages are proto2, so
//! scalar fields keep `Option<>` presence instead of prost's proto3 defaults;
//! the wire never sees a false-presence zero written by us.
//!
//! Two enum names deserve caution when reading upstream code:
//! `CHAT_MESSAGE_SOURCE_SYSTEM = 2` means assistant history, and
//! `CHAT_MESSAGE_SOURCE_TOOL = 4` means tool results. Assistant turns are
//! never merged into the top-level system prompt.

/// `ExaCodeiumCommonPb_ChatMessageSource`: user=1, assistant (SYSTEM)=2,
/// tool result=4, unspecified=0.
pub mod chat_message_source {
    pub const UNSPECIFIED: i32 = 0;
    pub const USER: i32 = 1;
    pub const SYSTEM: i32 = 2;
    pub const TOOL: i32 = 4;
}

/// `ExaCodeiumCommonPb_StopReason`.
pub mod stop_reason {
    pub const UNSPECIFIED: i32 = 0;
    pub const INCOMPLETE: i32 = 1;
    pub const MAX_TOKENS: i32 = 3;
    pub const PARTIAL: i32 = 9;
    pub const FUNCTION_CALL: i32 = 10;
    pub const ERROR: i32 = 13;
}

/// `ExaCodeiumCommonPb_ModelProvider`.
pub mod model_provider {
    pub const UNSPECIFIED: i32 = 0;
    pub const WINDSURF: i32 = 1;
    pub const OPENAI: i32 = 2;
    pub const ANTHROPIC: i32 = 3;
}

/// `ChatMessageRequestType`.
pub mod chat_message_request_type {
    pub const UNSPECIFIED: i32 = 0;
    pub const CASCADE: i32 = 5;
}

/// `ExaCodeiumCommonPb_ConversationalPlannerMode`.
pub mod planner_mode {
    pub const UNSPECIFIED: i32 = 0;
    pub const DEFAULT: i32 = 1;
}

/// `ExaCortexPb_CortexTrajectoryType`.
pub mod trajectory_type {
    pub const UNSPECIFIED: i32 = 0;
    pub const CASCADE: i32 = 4;
}

/// `ExaCortexPb_CortexStepType`.
pub mod step_type {
    pub const UNSPECIFIED: i32 = 0;
    pub const USER_INPUT: i32 = 14;
}

/// `GoogleProtobuf_Timestamp`.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Timestamp {
    #[prost(int64, optional, tag = "1")]
    pub seconds: Option<i64>,
    #[prost(int32, optional, tag = "2")]
    pub nanos: Option<i32>,
}

/// `ExaCodeiumCommonPb_Metadata`. The session token rides in `api_key = 3`
/// inside the protobuf body as well as in the Authorization header.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Metadata {
    #[prost(string, optional, tag = "1")]
    pub ide_name: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub extension_version: Option<String>,
    #[prost(string, optional, tag = "3")]
    pub api_key: Option<String>,
    #[prost(string, optional, tag = "4")]
    pub locale: Option<String>,
    #[prost(string, optional, tag = "5")]
    pub os: Option<String>,
    #[prost(string, optional, tag = "7")]
    pub ide_version: Option<String>,
    #[prost(string, optional, tag = "12")]
    pub extension_name: Option<String>,
    #[prost(string, optional, tag = "31")]
    pub f: Option<String>,
}

/// `ExaCodeiumCommonPb_CompletionConfiguration`.
#[derive(Clone, PartialEq, prost::Message)]
pub struct CompletionConfiguration {
    #[prost(uint64, optional, tag = "1")]
    pub num_completions: Option<u64>,
    #[prost(uint64, optional, tag = "2")]
    pub max_tokens: Option<u64>,
    #[prost(uint64, optional, tag = "3")]
    pub max_newlines: Option<u64>,
    #[prost(double, optional, tag = "5")]
    pub temperature: Option<f64>,
    #[prost(uint64, optional, tag = "7")]
    pub top_k: Option<u64>,
    #[prost(double, optional, tag = "8")]
    pub top_p: Option<f64>,
}

/// `ExaCodeiumCommonPb_ImageData`.
#[derive(Clone, PartialEq, prost::Message)]
pub struct ImageData {
    #[prost(string, optional, tag = "1")]
    pub base64_data: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub mime_type: Option<String>,
}

/// `ExaCodeiumCommonPb_ChatToolCall`: delta frames in responses, complete
/// calls in historical assistant turns.
#[derive(Clone, PartialEq, prost::Message)]
pub struct ChatToolCall {
    #[prost(string, optional, tag = "1")]
    pub id: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub name: Option<String>,
    #[prost(string, optional, tag = "3")]
    pub arguments_json: Option<String>,
}

/// `ExaChatPb_ChatToolDefinition`. The JSON schema string is passed through
/// verbatim (descriptions and annotations preserved).
#[derive(Clone, PartialEq, prost::Message)]
pub struct ChatToolDefinition {
    #[prost(string, optional, tag = "1")]
    pub name: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub description: Option<String>,
    #[prost(string, optional, tag = "3")]
    pub json_schema_string: Option<String>,
}

/// `ExaChatPb_ChatMessagePrompt`.
#[derive(Clone, PartialEq, prost::Message)]
pub struct ChatMessagePrompt {
    #[prost(string, optional, tag = "1")]
    pub message_id: Option<String>,
    #[prost(int32, optional, tag = "2")]
    pub source: Option<i32>,
    #[prost(string, optional, tag = "3")]
    pub prompt: Option<String>,
    #[prost(message, repeated, tag = "6")]
    pub tool_calls: Vec<ChatToolCall>,
    #[prost(string, optional, tag = "7")]
    pub tool_call_id: Option<String>,
    #[prost(bool, optional, tag = "9")]
    pub tool_result_is_error: Option<bool>,
    #[prost(message, repeated, tag = "10")]
    pub images: Vec<ImageData>,
    #[prost(string, optional, tag = "11")]
    pub thinking: Option<String>,
    #[prost(string, optional, tag = "12")]
    pub signature: Option<String>,
    #[prost(bool, optional, tag = "13")]
    pub thinking_redacted: Option<bool>,
}

/// `ExaCortexPb_CortexTrajectoryReference`.
#[derive(Clone, PartialEq, prost::Message)]
pub struct CortexTrajectoryReference {
    #[prost(string, optional, tag = "1")]
    pub trajectory_id: Option<String>,
    #[prost(int32, optional, tag = "3")]
    pub trajectory_type: Option<i32>,
    #[prost(int32, optional, tag = "4")]
    pub step_type: Option<i32>,
}

/// `ExaCodeiumCommonPb_ModelUsageStats`. Cache read and write are separate
/// counters; the total must never add them on top of input+output twice.
#[derive(Clone, PartialEq, prost::Message)]
pub struct ModelUsageStats {
    #[prost(uint64, optional, tag = "2")]
    pub input_tokens: Option<u64>,
    #[prost(uint64, optional, tag = "3")]
    pub output_tokens: Option<u64>,
    #[prost(uint64, optional, tag = "4")]
    pub cache_write_tokens: Option<u64>,
    #[prost(uint64, optional, tag = "5")]
    pub cache_read_tokens: Option<u64>,
    #[prost(string, optional, tag = "9")]
    pub model_uid: Option<String>,
}

/// `ExaCodeiumCommonPb_PromoStatus`.
#[derive(Clone, PartialEq, prost::Message)]
pub struct PromoStatus {
    #[prost(bool, optional, tag = "1")]
    pub is_active: Option<bool>,
    #[prost(message, optional, tag = "2")]
    pub end_date: Option<Timestamp>,
    #[prost(string, optional, tag = "3")]
    pub label: Option<String>,
}

/// `ExaCodeiumCommonPb_ModelFamilyMetadata`.
#[derive(Clone, PartialEq, prost::Message)]
pub struct ModelFamilyMetadata {
    #[prost(string, optional, tag = "1")]
    pub model_family_label: Option<String>,
    #[prost(bool, optional, tag = "3")]
    pub is_default_model_in_family: Option<bool>,
}

/// `ExaCodeiumCommonPb_ClientModelConfig` (model discovery response entries).
#[derive(Clone, PartialEq, prost::Message)]
pub struct ClientModelConfig {
    #[prost(string, optional, tag = "1")]
    pub label: Option<String>,
    #[prost(float, optional, tag = "3")]
    pub credit_multiplier: Option<f32>,
    #[prost(bool, optional, tag = "4")]
    pub disabled: Option<bool>,
    #[prost(bool, optional, tag = "5")]
    pub supports_images: Option<bool>,
    #[prost(bool, optional, tag = "7")]
    pub is_premium: Option<bool>,
    #[prost(bool, optional, tag = "9")]
    pub is_beta: Option<bool>,
    #[prost(int32, optional, tag = "10")]
    pub provider: Option<i32>,
    #[prost(bool, optional, tag = "11")]
    pub is_recommended: Option<bool>,
    #[prost(bool, optional, tag = "15")]
    pub is_new: Option<bool>,
    #[prost(int32, optional, tag = "18")]
    pub max_tokens: Option<i32>,
    #[prost(message, optional, tag = "19")]
    pub promo_status: Option<PromoStatus>,
    #[prost(bool, optional, tag = "20")]
    pub is_capacity_limited: Option<bool>,
    #[prost(string, optional, tag = "22")]
    pub model_uid: Option<String>,
    #[prost(string, optional, tag = "27")]
    pub description: Option<String>,
    #[prost(message, optional, tag = "30")]
    pub model_family_metadata: Option<ModelFamilyMetadata>,
}

/// `GetChatMessageRequest`. Streaming RPC; the Connect body carries the
/// 5-byte envelope, unlike the unary model-config request.
#[derive(Clone, PartialEq, prost::Message)]
pub struct GetChatMessageRequest {
    #[prost(message, optional, tag = "1")]
    pub metadata: Option<Metadata>,
    #[prost(string, optional, tag = "2")]
    pub prompt: Option<String>,
    #[prost(message, repeated, tag = "3")]
    pub chat_message_prompts: Vec<ChatMessagePrompt>,
    #[prost(int32, optional, tag = "7")]
    pub request_type: Option<i32>,
    #[prost(message, optional, tag = "8")]
    pub configuration: Option<CompletionConfiguration>,
    #[prost(message, repeated, tag = "10")]
    pub tools: Vec<ChatToolDefinition>,
    #[prost(message, optional, tag = "15")]
    pub trajectory_reference: Option<CortexTrajectoryReference>,
    #[prost(string, optional, tag = "16")]
    pub cascade_id: Option<String>,
    #[prost(int32, optional, tag = "20")]
    pub planner_mode: Option<i32>,
    #[prost(string, optional, tag = "21")]
    pub chat_model_uid: Option<String>,
    #[prost(string, optional, tag = "22")]
    pub execution_id: Option<String>,
}

/// `GetChatMessageResponse`.
#[derive(Clone, PartialEq, prost::Message)]
pub struct ChatMessageResponse {
    #[prost(string, optional, tag = "1")]
    pub message_id: Option<String>,
    #[prost(message, optional, tag = "2")]
    pub timestamp: Option<Timestamp>,
    #[prost(string, optional, tag = "3")]
    pub delta_text: Option<String>,
    #[prost(int32, optional, tag = "5")]
    pub stop_reason: Option<i32>,
    #[prost(message, repeated, tag = "6")]
    pub delta_tool_calls: Vec<ChatToolCall>,
    #[prost(message, optional, tag = "7")]
    pub usage: Option<ModelUsageStats>,
    #[prost(string, optional, tag = "9")]
    pub delta_thinking: Option<String>,
    #[prost(string, optional, tag = "10")]
    pub delta_signature: Option<String>,
    #[prost(bool, optional, tag = "11")]
    pub thinking_redacted: Option<bool>,
    #[prost(string, optional, tag = "23")]
    pub actual_model_uid: Option<String>,
}

/// `GetCascadeModelConfigsRequest` (unary).
#[derive(Clone, PartialEq, prost::Message)]
pub struct GetCascadeModelConfigsRequest {
    #[prost(message, optional, tag = "1")]
    pub metadata: Option<Metadata>,
}

/// `GetCascadeModelConfigsResponse` (unary).
#[derive(Clone, PartialEq, prost::Message)]
pub struct GetCascadeModelConfigsResponse {
    #[prost(message, repeated, tag = "1")]
    pub client_model_configs: Vec<ClientModelConfig>,
}
