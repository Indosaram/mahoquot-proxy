use serde::{Deserialize, Serialize};
use std::ffi::{c_char, c_void};

pub const ABI_VERSION: u32 = 1;

pub const SCHEMA_VERSION: u32 = 6;
pub const SCHEMA_VERSION_STREAM_CHUNK_OMIT_REQUEST_BODY: u32 = 3;
pub const SCHEMA_VERSION_WEBSOCKET_RESPONSE_OBSERVER: u32 = 4;
pub const SCHEMA_VERSION_STREAM_CHUNK_OMIT_HISTORY: u32 = 5;
pub const SCHEMA_VERSION_RAW_MANAGEMENT_RESPONSE: u32 = 6;

pub const METHOD_PLUGIN_REGISTER: &str = "plugin.register";
pub const METHOD_PLUGIN_QUIESCE: &str = "plugin.quiesce";
pub const METHOD_PLUGIN_RECONFIGURE: &str = "plugin.reconfigure";
pub const METHOD_PLUGIN_SHUTDOWN: &str = "plugin.shutdown";

pub const METHOD_MODEL_REGISTER: &str = "model.register";
pub const METHOD_MODEL_STATIC: &str = "model.static";
pub const METHOD_MODEL_FOR_AUTH: &str = "model.for_auth";

pub const METHOD_AUTH_IDENTIFIER: &str = "auth.identifier";
pub const METHOD_AUTH_PARSE: &str = "auth.parse";
pub const METHOD_AUTH_LOGIN_START: &str = "auth.login.start";
pub const METHOD_AUTH_LOGIN_POLL: &str = "auth.login.poll";
pub const METHOD_AUTH_REFRESH: &str = "auth.refresh";

pub const METHOD_FRONTEND_AUTH_IDENTIFIER: &str = "frontend_auth.identifier";
pub const METHOD_FRONTEND_AUTH_AUTHENTICATE: &str = "frontend_auth.authenticate";

pub const METHOD_SCHEDULER_PICK: &str = "scheduler.pick";
pub const METHOD_MODEL_ROUTE: &str = "model.route";

pub const METHOD_EXECUTOR_IDENTIFIER: &str = "executor.identifier";
pub const METHOD_EXECUTOR_EXECUTE: &str = "executor.execute";
pub const METHOD_EXECUTOR_EXECUTE_STREAM: &str = "executor.execute_stream";
pub const METHOD_EXECUTOR_COUNT_TOKENS: &str = "executor.count_tokens";
pub const METHOD_EXECUTOR_HTTP_REQUEST: &str = "executor.http_request";

pub const METHOD_REQUEST_TRANSLATE: &str = "request.translate";
pub const METHOD_REQUEST_NORMALIZE: &str = "request.normalize";
pub const METHOD_REQUEST_INTERCEPT_BEFORE: &str = "request.intercept_before";
pub const METHOD_REQUEST_INTERCEPT_AFTER: &str = "request.intercept_after";
pub const METHOD_REQUEST_COMPLETE: &str = "request.complete";

pub const METHOD_RESPONSE_TRANSLATE: &str = "response.translate";
pub const METHOD_RESPONSE_NORMALIZE_BEFORE: &str = "response.normalize_before";
pub const METHOD_RESPONSE_NORMALIZE_AFTER: &str = "response.normalize_after";
pub const METHOD_RESPONSE_INTERCEPT_AFTER: &str = "response.intercept_after";
pub const METHOD_RESPONSE_INTERCEPT_STREAM_CHUNK: &str = "response.intercept_stream_chunk";

pub const METHOD_WEBSOCKET_RESPONSE_EVENT: &str = "websocket.response_event";

pub const METHOD_THINKING_IDENTIFIER: &str = "thinking.identifier";
pub const METHOD_THINKING_APPLY: &str = "thinking.apply";

pub const METHOD_USAGE_HANDLE: &str = "usage.handle";

pub const METHOD_COMMAND_LINE_REGISTER: &str = "command_line.register";
pub const METHOD_COMMAND_LINE_EXECUTE: &str = "command_line.execute";

pub const METHOD_MANAGEMENT_REGISTER: &str = "management.register";
pub const METHOD_MANAGEMENT_HANDLE: &str = "management.handle";

pub const METHOD_QUOTA_IDENTIFIER: &str = "quota.identifier";
pub const METHOD_QUOTA_DESCRIBE: &str = "quota.describe";
pub const METHOD_QUOTA_FETCH: &str = "quota.fetch";
pub const METHOD_QUOTA_RESET: &str = "quota.reset";

pub const METHOD_HOST_HTTP_DO: &str = "host.http.do";
pub const METHOD_HOST_HTTP_DO_STREAM: &str = "host.http.do_stream";
pub const METHOD_HOST_HTTP_OPERATION_OPEN: &str = "host.http.operation_open";
pub const METHOD_HOST_HTTP_CANCEL: &str = "host.http.cancel";
pub const METHOD_HOST_HTTP_STREAM_READ: &str = "host.http.stream_read";
pub const METHOD_HOST_HTTP_STREAM_CLOSE: &str = "host.http.stream_close";
pub const METHOD_HOST_MODEL_EXECUTE: &str = "host.model.execute";
pub const METHOD_HOST_MODEL_EXECUTE_STREAM: &str = "host.model.execute_stream";
pub const METHOD_HOST_MODEL_STREAM_READ: &str = "host.model.stream_read";
pub const METHOD_HOST_MODEL_STREAM_CLOSE: &str = "host.model.stream_close";
pub const METHOD_HOST_STREAM_EMIT: &str = "host.stream.emit";
pub const METHOD_HOST_STREAM_CLOSE: &str = "host.stream.close";
pub const METHOD_HOST_LOG: &str = "host.log";
pub const METHOD_HOST_AUTH_LIST: &str = "host.auth.list";
pub const METHOD_HOST_AUTH_GET: &str = "host.auth.get";
pub const METHOD_HOST_AUTH_GET_RUNTIME: &str = "host.auth.get_runtime";
pub const METHOD_HOST_AUTH_SAVE: &str = "host.auth.save";
pub const METHOD_HOST_AFFINITY_LOOKUP: &str = "host.affinity.lookup";
pub const METHOD_HOST_ROUTING_RESET_COOLDOWN: &str = "host.routing.reset_cooldown";

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct CliproxyBuffer {
    pub ptr: *mut c_void,
    pub len: usize,
}

impl Default for CliproxyBuffer {
    fn default() -> Self {
        Self {
            ptr: std::ptr::null_mut(),
            len: 0,
        }
    }
}

pub type CliproxyHostCallFn = unsafe extern "C" fn(
    host_ctx: *mut c_void,
    method: *const c_char,
    request: *const u8,
    request_len: usize,
    response: *mut CliproxyBuffer,
) -> i32;

pub type CliproxyHostFreeFn = unsafe extern "C" fn(ptr: *mut c_void, len: usize);

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct CliproxyHostApi {
    pub abi_version: u32,
    pub host_ctx: *mut c_void,
    pub call: Option<CliproxyHostCallFn>,
    pub free_buffer: Option<CliproxyHostFreeFn>,
}

pub type CliproxyPluginCallFn = unsafe extern "C" fn(
    method: *const c_char,
    request: *const u8,
    request_len: usize,
    response: *mut CliproxyBuffer,
) -> i32;

pub type CliproxyPluginFreeFn = unsafe extern "C" fn(ptr: *mut c_void, len: usize);
pub type CliproxyPluginShutdownFn = unsafe extern "C" fn();

#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct CliproxyPluginApi {
    pub abi_version: u32,
    pub call: Option<CliproxyPluginCallFn>,
    pub free_buffer: Option<CliproxyPluginFreeFn>,
    pub shutdown: Option<CliproxyPluginShutdownFn>,
}

pub type CliproxyPluginInitFn =
    unsafe extern "C" fn(host: *const CliproxyHostApi, plugin: *mut CliproxyPluginApi) -> i32;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<PluginError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginError {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for PluginError {}

impl Envelope {
    pub fn success(result: serde_json::Value) -> Self {
        Self {
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(code: impl Into<String>, message: impl Into<String>, status: Option<u16>) -> Self {
        Self {
            ok: false,
            result: None,
            error: Some(PluginError {
                code: code.into(),
                message: message.into(),
                retryable: false,
                http_status: status,
            }),
        }
    }
}
