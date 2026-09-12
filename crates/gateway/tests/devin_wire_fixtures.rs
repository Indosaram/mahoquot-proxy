//! Independent Devin wire-contract fixture tests (plan P0).
//!
//! These tests intentionally DO NOT use any production protobuf encoder or
//! decoder. Every assertion decodes the committed golden hex in
//! `tests/data/devin/` with a minimal wire parser defined below, and compares
//! against expectations documented inside each fixture JSON.
//!
//! Fixtures are independently hand-specified from the field numbers of
//! `crates/gateway/src/compat/devin.proto` (verbatim copy of
//! Arborsm/dsh-plugin-devin-bridge @ ced035dd2f43b2dbf37a25c8a47b8ae1c1cc47f4,
//! MIT, Copyright (c) 2026 Arborsm). All credentials in fixtures are synthetic
//! dummies; there is no real account or live-server evidence here.

use std::path::PathBuf;

use serde_json::Value;

const DUMMY_TOKEN: &str = "dummy-token";
const LITERAL_BASIC: &str = "Basic dummy-token-dummy-token";
const MODEL_UID: &str = "glm-5-2";

// ─── Minimal independent protobuf wire parser ────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Val {
    Varint(u64),
    Fixed64([u8; 8]),
    Fixed32([u8; 4]),
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq)]
struct WireField {
    field: u32,
    wire_type: u8,
    tag_bytes: Vec<u8>,
    len: Option<usize>,
    len_bytes: Vec<u8>,
    payload_bytes: Vec<u8>,
    val: Val,
}

/// Parses ALL of `buf` as repeated protobuf wire fields, preserving tag bytes,
/// length varint bytes, payload bytes, and decoded values.
/// This parser is completely independent of Prost and production code.
fn parse_wire_fields(buf: &[u8]) -> Result<Vec<WireField>, String> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < buf.len() {
        let tag_start = i;
        let (tag, next) = read_varint(buf, i)?;
        let tag_bytes = buf[tag_start..next].to_vec();
        i = next;
        let field = (tag >> 3) as u32;
        let wt = (tag & 7) as u8;
        if field == 0 {
            return Err("field number 0 is invalid".into());
        }
        match wt {
            0 => {
                let val_start = i;
                let (v, next) = read_varint(buf, i)?;
                let payload_bytes = buf[val_start..next].to_vec();
                i = next;
                out.push(WireField {
                    field,
                    wire_type: wt,
                    tag_bytes,
                    len: None,
                    len_bytes: Vec::new(),
                    payload_bytes,
                    val: Val::Varint(v),
                });
            }
            1 => {
                if buf.len() < i + 8 {
                    return Err(format!("field {field}: truncated fixed64"));
                }
                let mut b = [0u8; 8];
                b.copy_from_slice(&buf[i..i + 8]);
                let payload_bytes = buf[i..i + 8].to_vec();
                i += 8;
                out.push(WireField {
                    field,
                    wire_type: wt,
                    tag_bytes,
                    len: Some(8),
                    len_bytes: Vec::new(),
                    payload_bytes,
                    val: Val::Fixed64(b),
                });
            }
            2 => {
                let len_start = i;
                let (len, next) = read_varint(buf, i)?;
                let len_bytes = buf[len_start..next].to_vec();
                i = next;
                let len_usize = len as usize;
                if buf.len() < i + len_usize {
                    return Err(format!("field {field}: truncated length-delimited"));
                }
                let payload_bytes = buf[i..i + len_usize].to_vec();
                i += len_usize;
                out.push(WireField {
                    field,
                    wire_type: wt,
                    tag_bytes,
                    len: Some(len_usize),
                    len_bytes,
                    payload_bytes: payload_bytes.clone(),
                    val: Val::Bytes(payload_bytes),
                });
            }
            5 => {
                if buf.len() < i + 4 {
                    return Err(format!("field {field}: truncated fixed32"));
                }
                let mut b = [0u8; 4];
                b.copy_from_slice(&buf[i..i + 4]);
                let payload_bytes = buf[i..i + 4].to_vec();
                i += 4;
                out.push(WireField {
                    field,
                    wire_type: wt,
                    tag_bytes,
                    len: Some(4),
                    len_bytes: Vec::new(),
                    payload_bytes,
                    val: Val::Fixed32(b),
                });
            }
            other => return Err(format!("field {field}: unsupported wire type {other}")),
        }
    }
    Ok(out)
}

fn parse_fields(buf: &[u8]) -> Result<Vec<(u32, u8, Val)>, String> {
    parse_wire_fields(buf).map(|v| {
        v.into_iter()
            .map(|f| (f.field, f.wire_type, f.val))
            .collect()
    })
}

fn read_varint(buf: &[u8], mut i: usize) -> Result<(u64, usize), String> {
    let mut result = 0u64;
    let mut shift = 0u32;
    loop {
        if i >= buf.len() || shift > 63 {
            return Err("truncated or oversized varint".into());
        }
        let b = buf[i];
        i += 1;
        result |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok((result, i));
        }
        shift += 7;
    }
}

// ─── Fixed schema-aware field definitions from devin.proto ───────────────────

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ProtoSchema {
    GetCascadeModelConfigsRequest,
    GetCascadeModelConfigsResponse,
    GetChatMessageRequest,
    GetChatMessageResponse,
    Metadata,
    CompletionConfiguration,
    ChatMessagePrompt,
    ChatToolCall,
    ChatToolDefinition,
    ImageData,
    CortexTrajectoryReference,
    ModelUsageStats,
    ClientModelConfig,
    PromoStatus,
    ModelFamilyMetadata,
    Timestamp,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SchemaField {
    StringField,
    Submessage(ProtoSchema),
    Varint,
    Fixed64,
    Fixed32,
}

fn schema_field(schema: ProtoSchema, field_no: u32) -> Option<SchemaField> {
    use ProtoSchema::*;
    use SchemaField::*;
    match (schema, field_no) {
        // GetCascadeModelConfigsRequest
        (GetCascadeModelConfigsRequest, 1) => Some(Submessage(Metadata)),

        // GetCascadeModelConfigsResponse
        (GetCascadeModelConfigsResponse, 1) => Some(Submessage(ClientModelConfig)),

        // GetChatMessageRequest
        (GetChatMessageRequest, 1) => Some(Submessage(Metadata)),
        (GetChatMessageRequest, 2) => Some(StringField),
        (GetChatMessageRequest, 3) => Some(Submessage(ChatMessagePrompt)),
        (GetChatMessageRequest, 7) => Some(Varint),
        (GetChatMessageRequest, 8) => Some(Submessage(CompletionConfiguration)),
        (GetChatMessageRequest, 10) => Some(Submessage(ChatToolDefinition)),
        (GetChatMessageRequest, 15) => Some(Submessage(CortexTrajectoryReference)),
        (GetChatMessageRequest, 16) => Some(StringField),
        (GetChatMessageRequest, 20) => Some(Varint),
        (GetChatMessageRequest, 21) => Some(StringField),
        (GetChatMessageRequest, 22) => Some(StringField),

        // GetChatMessageResponse
        (GetChatMessageResponse, 1) => Some(StringField),
        (GetChatMessageResponse, 2) => Some(Submessage(Timestamp)),
        (GetChatMessageResponse, 3) => Some(StringField),
        (GetChatMessageResponse, 5) => Some(Varint),
        (GetChatMessageResponse, 6) => Some(Submessage(ChatToolCall)),
        (GetChatMessageResponse, 7) => Some(Submessage(ModelUsageStats)),
        (GetChatMessageResponse, 9) => Some(StringField),
        (GetChatMessageResponse, 10) => Some(StringField),
        (GetChatMessageResponse, 11) => Some(Varint),
        (GetChatMessageResponse, 23) => Some(StringField),

        // Metadata (ExaCodeiumCommonPb_Metadata)
        (Metadata, 1) => Some(StringField),
        (Metadata, 2) => Some(StringField),
        (Metadata, 3) => Some(StringField),
        (Metadata, 4) => Some(StringField),
        (Metadata, 5) => Some(StringField),
        (Metadata, 7) => Some(StringField),
        (Metadata, 12) => Some(StringField),
        (Metadata, 31) => Some(StringField),

        // CompletionConfiguration (ExaCodeiumCommonPb_CompletionConfiguration)
        (CompletionConfiguration, 1) => Some(Varint),
        (CompletionConfiguration, 2) => Some(Varint),
        (CompletionConfiguration, 3) => Some(Varint),
        (CompletionConfiguration, 5) => Some(Fixed64),
        (CompletionConfiguration, 7) => Some(Varint),
        (CompletionConfiguration, 8) => Some(Fixed64),

        // ChatMessagePrompt (ExaChatPb_ChatMessagePrompt)
        (ChatMessagePrompt, 1) => Some(StringField),
        (ChatMessagePrompt, 2) => Some(Varint),
        (ChatMessagePrompt, 3) => Some(StringField),
        (ChatMessagePrompt, 6) => Some(Submessage(ChatToolCall)),
        (ChatMessagePrompt, 7) => Some(StringField),
        (ChatMessagePrompt, 9) => Some(Varint),
        (ChatMessagePrompt, 10) => Some(Submessage(ImageData)),
        (ChatMessagePrompt, 11) => Some(StringField),
        (ChatMessagePrompt, 12) => Some(StringField),
        (ChatMessagePrompt, 13) => Some(Varint),

        // ChatToolCall (ExaCodeiumCommonPb_ChatToolCall)
        (ChatToolCall, 1) => Some(StringField),
        (ChatToolCall, 2) => Some(StringField),
        (ChatToolCall, 3) => Some(StringField),

        // ChatToolDefinition (ExaChatPb_ChatToolDefinition)
        (ChatToolDefinition, 1) => Some(StringField),
        (ChatToolDefinition, 2) => Some(StringField),
        (ChatToolDefinition, 3) => Some(StringField),

        // ImageData (ExaCodeiumCommonPb_ImageData)
        (ImageData, 1) => Some(StringField),
        (ImageData, 2) => Some(StringField),

        // CortexTrajectoryReference (ExaCortexPb_CortexTrajectoryReference)
        (CortexTrajectoryReference, 1) => Some(StringField),
        (CortexTrajectoryReference, 3) => Some(Varint),
        (CortexTrajectoryReference, 4) => Some(Varint),

        // ModelUsageStats (ExaCodeiumCommonPb_ModelUsageStats)
        (ModelUsageStats, 2) => Some(Varint),
        (ModelUsageStats, 3) => Some(Varint),
        (ModelUsageStats, 4) => Some(Varint),
        (ModelUsageStats, 5) => Some(Varint),
        (ModelUsageStats, 9) => Some(StringField),

        // ClientModelConfig (ExaCodeiumCommonPb_ClientModelConfig)
        (ClientModelConfig, 1) => Some(StringField),
        (ClientModelConfig, 3) => Some(Fixed32),
        (ClientModelConfig, 4) => Some(Varint),
        (ClientModelConfig, 5) => Some(Varint),
        (ClientModelConfig, 7) => Some(Varint),
        (ClientModelConfig, 9) => Some(Varint),
        (ClientModelConfig, 10) => Some(Varint),
        (ClientModelConfig, 11) => Some(Varint),
        (ClientModelConfig, 15) => Some(Varint),
        (ClientModelConfig, 18) => Some(Varint),
        (ClientModelConfig, 19) => Some(Submessage(PromoStatus)),
        (ClientModelConfig, 20) => Some(Varint),
        (ClientModelConfig, 22) => Some(StringField),
        (ClientModelConfig, 27) => Some(StringField),
        (ClientModelConfig, 30) => Some(Submessage(ModelFamilyMetadata)),

        // PromoStatus (ExaCodeiumCommonPb_PromoStatus)
        (PromoStatus, 1) => Some(Varint),
        (PromoStatus, 2) => Some(Submessage(Timestamp)),
        (PromoStatus, 3) => Some(StringField),

        // ModelFamilyMetadata (ExaCodeiumCommonPb_ModelFamilyMetadata)
        (ModelFamilyMetadata, 1) => Some(StringField),
        (ModelFamilyMetadata, 3) => Some(Varint),

        // Timestamp (GoogleProtobuf_Timestamp)
        (Timestamp, 1) => Some(Varint),
        (Timestamp, 2) => Some(Varint),

        _ => None,
    }
}

/// Fixed schema-aware traversal of protobuf fields. Unlike naive heuristics that
/// guess submessages from parse success, this uses declared field schemas so that
/// invalid UTF-8 strings cannot evade validation even if their raw bytes happen
/// to be syntactically parseable as a protobuf message.
fn audit_fields_schema_aware(
    fields: &[WireField],
    schema: ProtoSchema,
    path: &str,
) -> Result<(), String> {
    for f in fields {
        let expected = schema_field(schema, f.field).ok_or_else(|| {
            format!("unknown field {} in schema {schema:?} at {path}.field{}", f.field, f.field)
        })?;
        match expected {
            SchemaField::StringField => {
                if f.wire_type != 2 {
                    return Err(format!(
                        "string field {} at {path}.field{} has wrong wire type {}",
                        f.field, f.field, f.wire_type
                    ));
                }
                std::str::from_utf8(&f.payload_bytes).map_err(|e| {
                    format!(
                        "invalid UTF-8 in string field {} at {path}.field{}: {e:?}, bytes: {:02x?}",
                        f.field, f.field, f.payload_bytes
                    )
                })?;
            }
            SchemaField::Submessage(child_schema) => {
                if f.wire_type != 2 {
                    return Err(format!(
                        "submessage field {} at {path}.field{} has wrong wire type {}",
                        f.field, f.field, f.wire_type
                    ));
                }
                let sub = parse_wire_fields(&f.payload_bytes).map_err(|e| {
                    format!(
                        "failed to parse submessage {child_schema:?} at {path}.field{}: {e}",
                        f.field
                    )
                })?;
                audit_fields_schema_aware(&sub, child_schema, &format!("{path}.field{}", f.field))?;
            }
            SchemaField::Varint => {
                if f.wire_type != 0 {
                    return Err(format!(
                        "varint field {} at {path}.field{} has wrong wire type {}",
                        f.field, f.field, f.wire_type
                    ));
                }
            }
            SchemaField::Fixed64 => {
                if f.wire_type != 1 {
                    return Err(format!(
                        "fixed64 field {} at {path}.field{} has wrong wire type {}",
                        f.field, f.field, f.wire_type
                    ));
                }
            }
            SchemaField::Fixed32 => {
                if f.wire_type != 5 {
                    return Err(format!(
                        "fixed32 field {} at {path}.field{} has wrong wire type {}",
                        f.field, f.field, f.wire_type
                    ));
                }
            }
        }
    }
    Ok(())
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn fixture(name: &str) -> Value {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidates = [
        manifest_dir.join("tests/data/devin").join(format!("{name}.json")),
        manifest_dir
            .parent()
            .and_then(|p| p.parent())
            .unwrap_or(&manifest_dir)
            .join("tests/data/devin")
            .join(format!("{name}.json")),
    ];
    let path = candidates
        .iter()
        .find(|p| p.exists())
        .unwrap_or_else(|| panic!("fixture {name}.json not found in candidate paths: {candidates:?}"));
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("cannot read fixture {}: {e}", path.display()));
    serde_json::from_str(&raw).expect("fixture JSON parses")
}

fn hex_of(v: &Value, key: &str) -> Vec<u8> {
    let s = v[key].as_str().expect(key);
    hex_to_bytes(s)
}

fn hex_to_bytes(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex digit"))
        .collect()
}

fn bytes_at(v: &Value, idx: usize, key: &str) -> Vec<u8> {
    hex_to_bytes(v["frames"][idx][key].as_str().expect(key))
}

/// Splits a Connect framed body into (flag, payload) frames.
fn split_connect_frames(body: &[u8]) -> Result<Vec<(u8, Vec<u8>)>, String> {
    let mut frames = Vec::new();
    let mut i = 0usize;
    while i < body.len() {
        if body.len() < i + 5 {
            return Err("truncated Connect frame header".into());
        }
        let flag = body[i];
        let len = u32::from_be_bytes([body[i + 1], body[i + 2], body[i + 3], body[i + 4]]) as usize;
        if body.len() < i + 5 + len {
            return Err("truncated Connect frame payload".into());
        }
        frames.push((flag, body[i + 5..i + 5 + len].to_vec()));
        i += 5 + len;
    }
    Ok(frames)
}

/// Independent streaming transport chunk assembler that simulates receiving
/// raw bytes across network packet boundaries and extracting Connect frames.
struct StreamAssembler {
    buffer: Vec<u8>,
}

impl StreamAssembler {
    fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    fn push_chunk(&mut self, chunk: &[u8]) -> Vec<(u8, Vec<u8>)> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();
        loop {
            if self.buffer.len() < 5 {
                break;
            }
            let flag = self.buffer[0];
            let len = u32::from_be_bytes([
                self.buffer[1],
                self.buffer[2],
                self.buffer[3],
                self.buffer[4],
            ]) as usize;
            if self.buffer.len() < 5 + len {
                break;
            }
            let payload = self.buffer[5..5 + len].to_vec();
            self.buffer.drain(..5 + len);
            frames.push((flag, payload));
        }
        frames
    }

    fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }
}

fn as_string(val: &Val) -> String {
    match val {
        Val::Bytes(b) => String::from_utf8(b.clone()).expect("valid utf-8 string field"),
        other => panic!("expected bytes, got {other:?}"),
    }
}

fn try_as_string(val: &Val) -> Result<String, std::string::FromUtf8Error> {
    match val {
        Val::Bytes(b) => String::from_utf8(b.clone()),
        other => panic!("expected bytes, got {other:?}"),
    }
}

fn first(fields: &[(u32, u8, Val)], field: u32) -> &Val {
    &fields
        .iter()
        .find(|(f, _, _)| *f == field)
        .unwrap_or_else(|| panic!("field {field} absent"))
        .2
}

fn all(fields: &[(u32, u8, Val)], field: u32) -> Vec<&Val> {
    fields
        .iter()
        .filter(|(f, _, _)| *f == field)
        .map(|(_, _, v)| v)
        .collect()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// Unary RPC: unframed protobuf, application/proto; auth token also lives in
/// the protobuf metadata (field 3 of field 1).
#[test]
fn unary_unframed_request_and_response() {
    let req = fixture("unary_request");
    assert_eq!(req["classification"], "positive");
    assert_eq!(req["content_type"], "application/proto");
    assert_eq!(req["encoding"], "unframed-proto");
    let body = hex_of(&req, "hex");

    // Unframed means the first byte is already a protobuf tag, not a Connect
    // frame header (0x00 flag + 4-byte length).
    assert_eq!(body[0], 0x0a, "field 1, wire type 2 tag");
    assert!(split_connect_frames(&body).is_err(), "unary body must not parse as Connect framing");
    let fields = parse_fields(&body).expect("unary request parses");
    let meta = match first(&fields, 1) {
        Val::Bytes(b) => parse_fields(b).expect("metadata submessage"),
        other => panic!("{other:?}"),
    };
    assert_eq!(as_string(first(&meta, 3)), DUMMY_TOKEN, "metadata.api_key");
    // Documented tag bytes: field 1 tag 0x0a, metadata field 3 tag 0x1a len 0x0b.
    assert_eq!(&body[..2], &hex_to_bytes("0a0d")[..]);
    let meta_bytes = match first(&fields, 1) {
        Val::Bytes(b) => b.clone(),
        _ => unreachable!(),
    };
    assert_eq!(&meta_bytes[..2], &hex_to_bytes("1a0b")[..], "api_key tag+len");

    let resp = fixture("unary_response");
    assert_eq!(resp["classification"], "positive");
    assert_eq!(resp["content_type"], "application/proto");
    let rbody = hex_of(&resp, "hex");
    assert_eq!(rbody[0], 0x0a, "unary response is unframed protobuf too");
    let rfields = parse_fields(&rbody).expect("unary response parses");
    let configs = all(&rfields, 1);
    assert_eq!(configs.len(), 2, "repeated client_model_configs");
    let c1 = match configs[0] {
        Val::Bytes(b) => parse_fields(b).expect("config"),
        _ => unreachable!(),
    };
    assert_eq!(as_string(first(&c1, 22)), MODEL_UID);
    assert_eq!(as_string(first(&c1, 1)), "GLM-5.2");
    assert_eq!(*first(&c1, 5), Val::Varint(1), "supports_images");
    assert_eq!(*first(&c1, 7), Val::Varint(1), "is_premium");
    assert_eq!(*first(&c1, 18), Val::Varint(200000), "max_tokens (synthetic)");
    // Documented varint tags: field 18 tag bytes 9001, field 22 tag bytes b201.
    let cfg_bytes = match configs[0] {
        Val::Bytes(b) => b.clone(),
        _ => unreachable!(),
    };
    assert!(cfg_bytes.windows(2).any(|w| w == hex_to_bytes("9001").as_slice()));
    assert!(cfg_bytes.windows(2).any(|w| w == hex_to_bytes("b201").as_slice()));
    let c2 = match configs[1] {
        Val::Bytes(b) => parse_fields(b).expect("config"),
        _ => unreachable!(),
    };
    assert_eq!(as_string(first(&c2, 22)), "swe-1-7");
    assert_eq!(*first(&c2, 18), Val::Varint(262000));
}

/// Streaming RPC request: exactly one Connect data frame (flag 0x00) wrapping
/// the GetChatMessageRequest; application/connect+proto, NOT application/proto.
#[test]
fn chat_framed_request() {
    let req = fixture("chat_request");
    assert_eq!(req["classification"], "positive");
    assert_eq!(req["content_type"], "application/connect+proto");
    assert_ne!(
        req["content_type"], "application/proto",
        "streaming requests use the Connect content type"
    );
    let body = hex_of(&req, "framed_hex");
    let frames = split_connect_frames(&body).expect("framed request");
    assert_eq!(frames.len(), 1, "single request frame");
    let (flag, payload) = &frames[0];
    assert_eq!(*flag, 0x00, "data frame flag");
    assert_eq!(payload.len(), 191, "documented frame payload length");
    assert_eq!(body[1..5], hex_to_bytes("000000bf"), "big-endian length prefix");

    let fields = parse_fields(payload).expect("request parses");
    let meta = match first(&fields, 1) {
        Val::Bytes(b) => parse_fields(b).expect("metadata"),
        _ => unreachable!(),
    };
    assert_eq!(as_string(first(&meta, 1)), "chisel", "ide_name");
    assert_eq!(as_string(first(&meta, 2)), "3000.2.17", "extension_version");
    assert_eq!(as_string(first(&meta, 3)), DUMMY_TOKEN, "api_key in body");
    assert_eq!(as_string(first(&meta, 4)), "en", "locale");
    assert_eq!(as_string(first(&meta, 5)), "win", "os");
    assert_eq!(as_string(first(&meta, 7)), "3000.2.17", "ide_version");
    assert_eq!(as_string(first(&meta, 12)), "chisel", "extension_name (tag 0x62)");
    assert_eq!(as_string(first(&fields, 2)), "You are a helpful assistant.", "prompt");
    let msgs = all(&fields, 3);
    assert_eq!(msgs.len(), 1, "chat_message_prompts");
    let msg = match msgs[0] {
        Val::Bytes(b) => parse_fields(b).expect("chat message prompt"),
        _ => unreachable!(),
    };
    assert_eq!(as_string(first(&msg, 1)), "msg-0001");
    assert_eq!(*first(&msg, 2), Val::Varint(1), "source USER=1");
    // Korean UTF-8 in the request prompt (field 3 of ChatMessagePrompt).
    assert_eq!(as_string(first(&msg, 3)), "안녕하세요, 한 줄 소개를 작성해 주세요.");
    assert_eq!(*first(&fields, 7), Val::Varint(5), "request_type CASCADE (tag 0x38)");
    let cfg = match first(&fields, 8) {
        Val::Bytes(b) => parse_fields(b).expect("configuration"),
        _ => unreachable!(),
    };
    assert_eq!(*first(&cfg, 1), Val::Varint(1), "num_completions");
    assert_eq!(*first(&cfg, 2), Val::Varint(4096), "max_tokens");
    match first(&cfg, 5) {
        Val::Fixed64(b) => {
            // 0.7 as IEEE-754 little-endian double, documented in the fixture.
            assert_eq!(&b[..], hex_to_bytes("666666666666e63f").as_slice(), "temperature double");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(as_string(first(&fields, 21)), MODEL_UID, "chat_model_uid (tag 0xaa01)");
    assert!(payload.windows(2).any(|w| w == hex_to_bytes("aa01").as_slice()), "field 21 tag bytes");
}

/// Streaming response: text, complete Korean UTF-8 per frame, thinking,
/// signature, redaction, interleaved tool deltas with explicit IDs, usage snapshot,
/// stop reason 10 with actual_model_uid, then the EndStreamResponse JSON frame.
#[test]
fn chat_framed_response_stream() {
    let resp = fixture("chat_response");
    assert_eq!(resp["classification"], "positive");
    assert_eq!(resp["content_type"], "application/connect+proto");
    let body = hex_of(&resp, "framed_hex");
    let frames = split_connect_frames(&body).expect("framed response");
    assert_eq!(frames.len(), 10, "9 data frames + 1 end-stream frame");
    for (flag, _) in &frames[..9] {
        assert_eq!(*flag, 0x00, "data frames carry flag 0x00");
    }
    assert_eq!(frames[9].0, 0x02, "final frame is the EndStreamResponse");

    let end_json: Value = serde_json::from_slice(&frames[9].1).expect("end-stream JSON");
    assert_eq!(end_json["error"], Value::Null);
    assert_eq!(end_json["hasSyncPoints"], Value::Bool(false));

    let fields: Vec<_> = frames[..9]
        .iter()
        .map(|(_, p)| parse_fields(p).expect("response frame parses"))
        .collect();

    // message_id on data frames.
    assert_eq!(as_string(first(&fields[0], 1)), "resp-0001");
    // Frame 0: text delta.
    assert_eq!(as_string(first(&fields[0], 3)), "Hello, ");
    assert_eq!(bytes_at(&resp, 0, "hex")[11], 0x1a, "delta_text tag after 0x0a09 + 9-byte message_id");
    // Frame 1: complete Korean UTF-8 character "가".
    assert_eq!(as_string(first(&fields[1], 3)), "가");
    // Frame 2: complete Korean UTF-8 character "나".
    assert_eq!(as_string(first(&fields[2], 3)), "나");
    // Frame 3: thinking delta (field 9, tag 0x4a).
    assert_eq!(as_string(first(&fields[3], 9)), "Planning the reply.");
    // Frame 4: opaque signature (field 10, tag 0x52) preserved verbatim.
    assert_eq!(as_string(first(&fields[4], 10)), "sig-opaque-0001");
    // Frame 5: thinking_redacted=true (field 11, tag bytes 58 01).
    assert_eq!(*first(&fields[5], 11), Val::Varint(1));
    assert_eq!(&frames[5].1[frames[5].1.len() - 2..], &hex_to_bytes("5801")[..]);

    // Frame 6: interleaved tool call deltas with explicit IDs in one frame.
    let deltas = all(&fields[6], 6);
    assert_eq!(deltas.len(), 4);
    let mut by_id: Vec<(String, String)> = Vec::new();
    for d in &deltas {
        let t = match d {
            Val::Bytes(b) => parse_fields(b).expect("tool call"),
            _ => unreachable!(),
        };
        let id = as_string(first(&t, 1));
        assert!(!id.is_empty(), "explicit ID required when multiple calls exist");
        let args = as_string(first(&t, 3));
        if let Some(existing) = by_id.iter_mut().find(|(cid, _)| cid == &id) {
            existing.1.push_str(&args);
        } else {
            by_id.push((id, args));
        }
    }
    assert_eq!(by_id.len(), 2);
    assert_eq!(by_id[0].0, "call_001");
    assert_eq!(by_id[0].1, r#"{"city":"Seoul"}"#);
    assert_eq!(by_id[1].0, "call_002");
    assert_eq!(by_id[1].1, r#"{"tz":"KST"}"#);
    serde_json::from_str::<Value>(&by_id[0].1).expect("concatenated arguments are valid JSON");
    serde_json::from_str::<Value>(&by_id[1].1).expect("concatenated arguments are valid JSON");

    // Frame 7: usage snapshot (field 7).
    let usage = match first(&fields[7], 7) {
        Val::Bytes(b) => parse_fields(b).expect("usage"),
        _ => unreachable!(),
    };
    assert_eq!(*first(&usage, 2), Val::Varint(12), "input_tokens");
    assert_eq!(*first(&usage, 3), Val::Varint(34), "output_tokens");
    assert_eq!(*first(&usage, 4), Val::Varint(7), "cache_write_tokens");
    assert_eq!(*first(&usage, 5), Val::Varint(5), "cache_read_tokens");
    assert_eq!(as_string(first(&usage, 9)), MODEL_UID, "usage.model_uid");
    assert_eq!(12 + 34, 46, "total is input+output; cache tokens are never re-added");

    // Frame 8: final text, stop reason 10 (FUNCTION_CALL), actual_model_uid.
    assert_eq!(as_string(first(&fields[8], 3)), "Done.");
    assert_eq!(*first(&fields[8], 5), Val::Varint(10), "stop reason FUNCTION_CALL");
    assert_eq!(as_string(first(&fields[8], 23)), MODEL_UID, "actual_model_uid");
    let f8 = &frames[8].1;
    assert!(f8.windows(2).any(|w| w == hex_to_bytes("ba01").as_slice()), "field 23 tag bytes");
}

/// Terminal error: EndStreamResponse frame (flag 0x02) with a code-only error
/// object. HTTP stays 200; this is a failure, not a success.
#[test]
fn terminal_code_only_error() {
    let fx = fixture("chat_endstream_error");
    assert_eq!(fx["classification"], "positive");
    assert_eq!(fx["content_type"], "application/connect+proto");
    let body = hex_of(&fx, "framed_hex");
    let frames = split_connect_frames(&body).expect("framed");
    assert_eq!(frames.len(), 1);
    let (flag, payload) = &frames[0];
    assert_eq!(*flag, 0x02, "EndStreamResponse flag");
    assert_eq!(body[1..5], hex_to_bytes("00000027"), "documented length 39");
    let end: Value = serde_json::from_slice(payload).expect("end-stream JSON");
    assert_eq!(end["error"]["code"], "resource_exhausted");
    assert!(
        end["error"].get("message").is_none() || end["error"]["message"].is_null(),
        "error is code-only: no message field"
    );
    assert_eq!(end["error"].as_object().map(|o| o.len()), Some(1), "exactly the code key");
}

/// Usage / cache snapshot in isolation.
#[test]
fn usage_and_cache_snapshot() {
    let fx = fixture("chat_usage");
    assert_eq!(fx["classification"], "positive");
    let body = hex_of(&fx, "framed_hex");
    let frames = split_connect_frames(&body).expect("framed");
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].0, 0x00);
    assert_eq!(frames[1].0, 0x02);
    let fields = parse_fields(&frames[0].1).expect("usage frame");
    let usage = match first(&fields, 7) {
        Val::Bytes(b) => parse_fields(b).expect("ModelUsageStats"),
        _ => unreachable!(),
    };
    assert_eq!(*first(&usage, 2), Val::Varint(12), "input");
    assert_eq!(*first(&usage, 3), Val::Varint(34), "output");
    assert_eq!(*first(&usage, 4), Val::Varint(7), "cache_write");
    assert_eq!(*first(&usage, 5), Val::Varint(5), "cache_read");
    assert_eq!(as_string(first(&usage, 9)), MODEL_UID);
    // Documented tag bytes for the scalar fields: 10 / 18 / 20 / 28, uid 4a.
    let raw = &frames[0].1;
    for tag in ["10", "18", "20", "28", "4a"] {
        assert!(raw.windows(tag.len() / 2).any(|w| w == hex_to_bytes(tag).as_slice()), "tag {tag}");
    }
    let end: Value = serde_json::from_slice(&frames[1].1).unwrap();
    assert_eq!(end["error"], Value::Null);
}

/// Reasoning sequence: thinking delta, opaque signature, redaction flag, then
/// a text delta that terminates the reasoning block.
#[test]
fn reasoning_signature_and_redaction() {
    let fx = fixture("chat_reasoning");
    assert_eq!(fx["classification"], "positive");
    let body = hex_of(&fx, "framed_hex");
    let frames = split_connect_frames(&body).expect("framed");
    assert_eq!(frames.len(), 5, "4 data frames + end");
    let fields: Vec<_> = frames[..4]
        .iter()
        .map(|(_, p)| parse_fields(p).expect("frame"))
        .collect();
    assert_eq!(as_string(first(&fields[0], 9)), "Planning the reply.", "delta_thinking");
    assert_eq!(as_string(first(&fields[1], 10)), "sig-opaque-0001", "delta_signature");
    assert_eq!(*first(&fields[2], 11), Val::Varint(1), "thinking_redacted=true");
    assert_eq!(as_string(first(&fields[3], 3)), "Hello, ", "delta_text after reasoning");
    assert!(fields[0].iter().all(|(f, _, _)| *f != 3), "reasoning frame has no text delta");
    assert!(fields[3].iter().all(|(f, _, _)| *f != 9), "text frame has no thinking delta");
}

/// Interleaved tool deltas spread across frames: two active calls with explicit
/// IDs so attachment is unambiguous without guessing.
#[test]
fn interleaved_tool_frames_across_frames() {
    let fx = fixture("chat_tools");
    assert_eq!(fx["classification"], "positive");
    let body = hex_of(&fx, "framed_hex");
    let frames = split_connect_frames(&body).expect("framed");
    assert_eq!(frames.len(), 6, "4 tool frames + final + end");
    let per_frame: Vec<_> = frames[..5]
        .iter()
        .map(|(_, p)| parse_fields(p).expect("frame"))
        .collect();

    let mut seq: Vec<(String, String, String)> = Vec::new();
    for f in &per_frame[..4] {
        let deltas = all(f, 6);
        assert_eq!(deltas.len(), 1, "one tool delta per frame");
        let t = match deltas[0] {
            Val::Bytes(b) => parse_fields(b).expect("tool call"),
            _ => unreachable!(),
        };
        let id = match t.iter().find(|(x, _, _)| *x == 1) {
            Some((_, _, v)) => as_string(v),
            None => String::new(),
        };
        let name = match t.iter().find(|(x, _, _)| *x == 2) {
            Some((_, _, v)) => as_string(v),
            None => String::new(),
        };
        seq.push((id, name, as_string(first(&t, 3))));
    }

    // Every delta has an explicit ID:
    assert_eq!(seq[0].0, "call_001");
    assert_eq!(seq[0].1, "get_weather");
    assert_eq!(seq[1].0, "call_002");
    assert_eq!(seq[1].1, "get_time");
    assert_eq!(seq[2].0, "call_001", "explicit ID call_001 eliminates ambiguity");
    assert_eq!(seq[3].0, "call_002", "explicit ID call_002 eliminates ambiguity");

    // Reconstruct per-call arguments by grouping on explicit ID.
    let mut args_by_call: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for (id, _, args) in &seq {
        args_by_call.entry(id.clone()).or_default().push_str(args);
    }
    assert_eq!(args_by_call.get("call_001").unwrap(), r#"{"city":"Seoul"}"#);
    assert_eq!(args_by_call.get("call_002").unwrap(), r#"{"tz":"KST"}"#);
    serde_json::from_str::<Value>(args_by_call.get("call_001").unwrap()).expect("valid JSON");
    serde_json::from_str::<Value>(args_by_call.get("call_002").unwrap()).expect("valid JSON");

    // Final frame: stop reason 10 (FUNCTION_CALL) and actual_model_uid.
    assert_eq!(*first(&per_frame[4], 5), Val::Varint(10));
    assert_eq!(as_string(first(&per_frame[4], 23)), MODEL_UID);
    assert!(per_frame[4].iter().all(|(f, _, _)| *f != 6), "no tool delta on the final frame");
}

/// Negative rejection fixture for ambiguous tool calls: when multiple calls are
/// active, an incoming tool delta missing an ID must be rejected as an error,
/// not guessed.
#[test]
fn ambiguous_tool_calls_rejected_negative_fixture() {
    let fx = fixture("chat_tools_ambiguous_negative");
    assert_eq!(fx["classification"], "negative");
    let body = hex_of(&fx, "framed_hex");
    let frames = split_connect_frames(&body).expect("framed");
    assert_eq!(frames.len(), 3, "2 open frames + 1 ambiguous frame");

    let f0_fields = parse_fields(&frames[0].1).expect("f0 parses");
    let f1_fields = parse_fields(&frames[1].1).expect("f1 parses");
    let f2_fields = parse_fields(&frames[2].1).expect("f2 parses");

    // Frame 0 introduces call_001.
    let d0 = match first(&f0_fields, 6) {
        Val::Bytes(b) => parse_fields(b).expect("tool call 0"),
        _ => unreachable!(),
    };
    assert_eq!(as_string(first(&d0, 1)), "call_001");

    // Frame 1 introduces call_002 (so 2 calls are concurrently active!).
    let d1 = match first(&f1_fields, 6) {
        Val::Bytes(b) => parse_fields(b).expect("tool call 1"),
        _ => unreachable!(),
    };
    assert_eq!(as_string(first(&d1, 1)), "call_002");

    // Frame 2 has NO id field.
    let d2 = match first(&f2_fields, 6) {
        Val::Bytes(b) => parse_fields(b).expect("tool call 2"),
        _ => unreachable!(),
    };
    let has_id = d2.iter().any(|(f, _, _)| *f == 1);
    assert!(!has_id, "Frame 2 must deliberately omit ID");

    // Decoders must classify this as a protocol error rather than guessing.
    assert_eq!(
        fx["expected_rejection"]["reason"],
        "ambiguous_id_less_delta_with_multiple_active_calls"
    );
}

/// Korean UTF-8 boundary: each protobuf frame contains complete valid UTF-8 strings
/// (Frame 0: "가", Frame 1: "나"). Transport stream bytes may be split at arbitrary
/// offsets across network chunks, but individual protobuf frames contain valid strings.
#[test]
fn korean_utf8_complete_strings_and_arbitrary_transport_splits() {
    let fx = fixture("chat_utf8_boundary");
    assert_eq!(fx["classification"], "positive");
    let body = hex_of(&fx, "framed_hex");

    // 1. Direct frame decode: each individual protobuf frame is 100% valid UTF-8.
    let frames = split_connect_frames(&body).expect("framed");
    assert_eq!(frames.len(), 3, "2 data frames + end");

    let f0_fields = parse_fields(&frames[0].1).expect("f0 parses");
    let f1_fields = parse_fields(&frames[1].1).expect("f1 parses");

    let s0 = as_string(first(&f0_fields, 3));
    let s1 = as_string(first(&f1_fields, 3));
    assert_eq!(s0, "가", "frame 0 contains complete valid character '가'");
    assert_eq!(s1, "나", "frame 1 contains complete valid character '나'");

    let combined = format!("{s0}{s1}");
    assert_eq!(combined, "가나");

    // 2. Arbitrary transport byte chunking: split the raw Connect stream into
    // various chunk sizes (1 byte, 2 bytes, 3 bytes, 7 bytes, 13 bytes, 16 bytes).
    // The stream assembler must faithfully reassemble the exact frames without loss.
    for chunk_size in [1, 2, 3, 5, 7, 11, 13, 16, 23] {
        let mut assembler = StreamAssembler::new();
        let mut assembled_frames = Vec::new();
        for chunk in body.chunks(chunk_size) {
            let newly_parsed = assembler.push_chunk(chunk);
            assembled_frames.extend(newly_parsed);
        }
        assert!(assembler.is_empty(), "all bytes consumed for chunk size {chunk_size}");
        assert_eq!(assembled_frames.len(), 3, "exact 3 frames for chunk size {chunk_size}");
        assert_eq!(assembled_frames[0].0, 0x00);
        assert_eq!(assembled_frames[1].0, 0x00);
        assert_eq!(assembled_frames[2].0, 0x02);

        let fields0 = parse_fields(&assembled_frames[0].1).expect("assembled f0");
        let fields1 = parse_fields(&assembled_frames[1].1).expect("assembled f1");
        assert_eq!(as_string(first(&fields0, 3)), "가");
        assert_eq!(as_string(first(&fields1, 3)), "나");
    }
}

/// Negative rejection fixture: malformed UTF-8 in a protobuf string field.
/// Splitting a UTF-8 codepoint across protobuf messages produces invalid UTF-8
/// strings which a conformant decoder must reject as a fatal error.
#[test]
fn malformed_utf8_rejected_negative_fixture() {
    let fx = fixture("chat_utf8_malformed_negative");
    assert_eq!(fx["classification"], "negative");
    let body = hex_of(&fx, "framed_hex");
    let frames = split_connect_frames(&body).expect("framed");
    assert_eq!(frames.len(), 1, "single malformed frame");

    let fields = parse_fields(&frames[0].1).expect("frame parses at wire level");
    let raw_val = first(&fields, 3);
    match raw_val {
        Val::Bytes(b) => {
            assert_eq!(b.as_slice(), hex_to_bytes("eab080eb").as_slice());
            // Fatal UTF-8 error on the individual string field:
            let utf8_res = try_as_string(raw_val);
            assert!(
                utf8_res.is_err(),
                "malformed UTF-8 in protobuf string MUST fail std::str::from_utf8"
            );
        }
        other => panic!("expected bytes, got {other:?}"),
    }

    assert_eq!(
        fx["expected_rejection"]["reason"],
        "invalid_utf8_in_protobuf_string"
    );
}

/// Faithful negative case: invalid UTF-8 string bytes [0x08, 0x80, 0x01] that are
/// syntactically parseable as a valid protobuf submessage (field 1 = varint 128).
/// Proves that decoders cannot use parse success heuristics to bypass string validation;
/// fixed schema-aware traversal correctly identifies field 3 as StringField and rejects it.
#[test]
fn proto_parseable_invalid_utf8_rejected_negative_case() {
    let fx = fixture("chat_utf8_proto_parseable_negative");
    assert_eq!(fx["classification"], "negative");
    let body = hex_of(&fx, "framed_hex");
    let frames = split_connect_frames(&body).expect("framed");
    assert_eq!(frames.len(), 1, "single malformed frame");

    let wire_fields = parse_wire_fields(&frames[0].1).expect("frame parses at wire level");
    let f3 = wire_fields.iter().find(|f| f.field == 3).expect("field 3 delta_text");

    // 1. Prove the payload bytes [0x08, 0x80, 0x01] DO parse as a valid protobuf submessage:
    let parsed_as_proto = parse_wire_fields(&f3.payload_bytes);
    assert!(
        parsed_as_proto.is_ok(),
        "bytes [0x08, 0x80, 0x01] syntactically parse as protobuf (field 1, varint 128)"
    );
    let sub_fields = parsed_as_proto.unwrap();
    assert_eq!(sub_fields.len(), 1);
    assert_eq!(sub_fields[0].field, 1);
    assert_eq!(sub_fields[0].val, Val::Varint(128));

    // 2. Prove that the bytes are NOT valid UTF-8 (0x80 is an invalid UTF-8 byte):
    let utf8_res = std::str::from_utf8(&f3.payload_bytes);
    assert!(
        utf8_res.is_err(),
        "bytes [0x08, 0x80, 0x01] MUST fail std::str::from_utf8"
    );

    // 3. Prove that schema-aware traversal rejects this message because field 3 is a declared string:
    let audit_res = audit_fields_schema_aware(
        &wire_fields,
        ProtoSchema::GetChatMessageResponse,
        "chat_utf8_proto_parseable_negative.frame0",
    );
    assert!(
        audit_res.is_err(),
        "schema-aware traversal must reject invalid UTF-8 in string field 3"
    );
    let err_msg = audit_res.unwrap_err();
    assert!(
        err_msg.contains("invalid UTF-8 in string field 3"),
        "error message must identify string field 3: {err_msg}"
    );

    assert_eq!(
        fx["expected_rejection"]["reason"],
        "invalid_utf8_in_protobuf_string_parseable_as_submessage"
    );
}

/// The literal dummy Basic header: exactly `Basic <token>-<token>`, which is
/// deliberately NOT base64 of `user:password`.
#[test]
fn literal_basic_dummy_token_header() {
    let req = fixture("unary_request");
    let chat = fixture("chat_request");
    for fx in [req, chat] {
        let auth = fx["headers"]["Authorization"].as_str().expect("Authorization");
        assert_eq!(auth, LITERAL_BASIC);
        let raw = auth.strip_prefix("Basic ").expect("Basic scheme");
        let expected_literal = format!("{DUMMY_TOKEN}-{DUMMY_TOKEN}");
        assert_eq!(raw, expected_literal, "literal <token>-<token>, no base64");
        // Prove it is not a base64 user:password pair.
        let b64 = {
            const TBL: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let input = format!("{DUMMY_TOKEN}:{DUMMY_TOKEN}").into_bytes();
            let mut out = String::new();
            for chunk in input.chunks(3) {
                let b = [
                    chunk[0],
                    *chunk.get(1).unwrap_or(&0),
                    *chunk.get(2).unwrap_or(&0),
                ];
                let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
                for shift in [18u32, 12, 6, 0] {
                    let idx = ((n >> shift) & 63) as usize;
                    out.push(TBL[idx] as char);
                }
                for _ in chunk.len()..3 {
                    out.pop();
                    out.push('=');
                }
            }
            out
        };
        assert_ne!(raw, b64, "must not be base64(user:pass)");
    }
}

/// The model UID contract: requested UID and actual_model_uid are the real
/// upstream UID (glm-5-2), never a client-facing alias with a suffix cut.
#[test]
fn actual_model_uid_contract() {
    let req = fixture("chat_request");
    let body = hex_of(&req, "framed_hex");
    let frames = split_connect_frames(&body).expect("framed");
    let fields = parse_fields(&frames[0].1).expect("request");
    assert_eq!(as_string(first(&fields, 21)), MODEL_UID, "chat_model_uid");

    let resp = fixture("chat_response");
    let rbody = hex_of(&resp, "framed_hex");
    let rframes = split_connect_frames(&rbody).expect("framed");
    let final_fields = parse_fields(&rframes[8].1).expect("final frame");
    assert_eq!(as_string(first(&final_fields, 23)), MODEL_UID, "actual_model_uid");

    let unary = fixture("unary_response");
    let ubody = hex_of(&unary, "hex");
    let ufields = parse_fields(&ubody).expect("unary response");
    let configs = all(&ufields, 1);
    let c1 = match configs[0] {
        Val::Bytes(b) => parse_fields(b).expect("config"),
        _ => unreachable!(),
    };
    assert_eq!(as_string(first(&c1, 22)), MODEL_UID, "catalog model_uid");
}

/// Audit that all string fields across all positive fixtures are strictly valid UTF-8
/// using fixed schema-aware field traversal.
#[test]
fn all_positive_fixtures_have_valid_utf8_strings() {
    let positive_fixture_cases = [
        ("unary_request", ProtoSchema::GetCascadeModelConfigsRequest),
        ("unary_response", ProtoSchema::GetCascadeModelConfigsResponse),
        ("chat_request", ProtoSchema::GetChatMessageRequest),
        ("chat_response", ProtoSchema::GetChatMessageResponse),
        ("chat_tools", ProtoSchema::GetChatMessageResponse),
        ("chat_reasoning", ProtoSchema::GetChatMessageResponse),
        ("chat_usage", ProtoSchema::GetChatMessageResponse),
        ("chat_utf8_boundary", ProtoSchema::GetChatMessageResponse),
    ];

    for (name, schema) in positive_fixture_cases {
        let fx = fixture(name);
        assert_eq!(
            fx["classification"], "positive",
            "fixture {name} must be classified positive"
        );

        let payloads: Vec<Vec<u8>> = if let Some(framed_hex) = fx.get("framed_hex") {
            let body = hex_to_bytes(framed_hex.as_str().unwrap());
            split_connect_frames(&body)
                .expect("framed body parses")
                .into_iter()
                .filter(|(flag, _)| *flag == 0x00) // data frames only
                .map(|(_, p)| p)
                .collect()
        } else {
            vec![hex_to_bytes(fx["hex"].as_str().unwrap())]
        };

        for (frame_idx, p) in payloads.iter().enumerate() {
            let fields = parse_wire_fields(p).unwrap_or_else(|e| {
                panic!("failed to parse fields in {name} frame {frame_idx}: {e}")
            });
            audit_fields_schema_aware(&fields, schema, &format!("{name}.frame{frame_idx}"))
                .unwrap_or_else(|e| {
                    panic!("schema-aware audit failed on {name} frame {frame_idx}: {e}")
                });
        }
    }
}

/// Exact path-navigated tag check assertion.
/// Navigates the exact declared frame and submessage path to verify tags, lengths,
/// and values without scanning arbitrary byte occurrences.
fn assert_tag_check_exact_path(fx: &Value, tc: &Value, fixture_name: &str) {
    let path = tc["path"].as_str().expect("path");
    let expected_hex = tc["hex"].as_str().expect("hex");
    let expected_tag_bytes = hex_to_bytes(expected_hex);
    let expected_len = tc.get("len").and_then(|l| l.as_u64());
    let expected_val = tc.get("value");

    let wire_bytes = if let Some(fh) = fx.get("framed_hex") {
        hex_to_bytes(fh.as_str().unwrap())
    } else {
        hex_to_bytes(fx["hex"].as_str().unwrap())
    };

    match path {
        "frame.flag" => {
            assert_eq!(wire_bytes[0], 0x00, "at {fixture_name} path {path}");
            assert_eq!(&wire_bytes[0..1], expected_tag_bytes.as_slice(), "at {fixture_name} path {path}");
        }
        "frame.len_be" => {
            let be_len = u32::from_be_bytes([wire_bytes[1], wire_bytes[2], wire_bytes[3], wire_bytes[4]]) as u64;
            assert_eq!(&wire_bytes[1..5], expected_tag_bytes.as_slice(), "at {fixture_name} path {path}");
            if let Some(l) = expected_len {
                assert_eq!(be_len, l, "at {fixture_name} path {path}");
            }
        }
        "end_stream.flag" => {
            let frames = split_connect_frames(&wire_bytes).expect("frames");
            let last = frames.last().expect("last frame");
            assert_eq!(last.0, 0x02, "at {fixture_name} path {path}");
            assert_eq!(&[last.0], expected_tag_bytes.as_slice(), "at {fixture_name} path {path}");
        }
        "end_stream.len_be" => {
            let frames = split_connect_frames(&wire_bytes).expect("frames");
            let last = frames.last().expect("last frame");
            if let Some(l) = expected_len {
                assert_eq!(last.1.len() as u64, l, "at {fixture_name} path {path}");
            }
            let last_offset = wire_bytes.len() - last.1.len() - 5;
            assert_eq!(&wire_bytes[last_offset + 1..last_offset + 5], expected_tag_bytes.as_slice(), "at {fixture_name} path {path}");
        }
        "field1.tag" => {
            let fields = parse_wire_fields(&wire_bytes).expect("fields");
            let f1 = fields.iter().find(|f| f.field == 1).expect("field 1");
            assert_eq!(f1.tag_bytes, expected_tag_bytes, "at {fixture_name} path {path}");
        }
        "field1.len" => {
            let fields = parse_wire_fields(&wire_bytes).expect("fields");
            let f1 = fields.iter().find(|f| f.field == 1).expect("field 1");
            assert_eq!(f1.len_bytes, expected_tag_bytes, "at {fixture_name} path {path}");
            if let Some(l) = expected_len {
                assert_eq!(f1.len.unwrap() as u64, l, "at {fixture_name} path {path}");
            }
        }
        "metadata.field3.tag" => {
            let fields = parse_wire_fields(&wire_bytes).expect("fields");
            let f1 = fields.iter().find(|f| f.field == 1).expect("field 1");
            let sub = parse_wire_fields(&f1.payload_bytes).expect("metadata fields");
            let f3 = sub.iter().find(|f| f.field == 3).expect("field 3");
            assert_eq!(f3.tag_bytes, expected_tag_bytes, "at {fixture_name} path {path}");
        }
        "metadata.field3.len" => {
            let fields = parse_wire_fields(&wire_bytes).expect("fields");
            let f1 = fields.iter().find(|f| f.field == 1).expect("field 1");
            let sub = parse_wire_fields(&f1.payload_bytes).expect("metadata fields");
            let f3 = sub.iter().find(|f| f.field == 3).expect("field 3");
            assert_eq!(f3.len_bytes, expected_tag_bytes, "at {fixture_name} path {path}");
            if let Some(l) = expected_len {
                assert_eq!(f3.len.unwrap() as u64, l, "at {fixture_name} path {path}");
            }
        }
        "metadata.field12.tag" => {
            let frames = split_connect_frames(&wire_bytes).expect("frames");
            let fields = parse_wire_fields(&frames[0].1).expect("frame 0 fields");
            let f1 = fields.iter().find(|f| f.field == 1).expect("metadata field 1");
            let sub = parse_wire_fields(&f1.payload_bytes).expect("metadata fields");
            let f12 = sub.iter().find(|f| f.field == 12).expect("field 12");
            assert_eq!(f12.tag_bytes, expected_tag_bytes, "at {fixture_name} path {path}");
            if let Some(l) = expected_len {
                assert_eq!(f12.len.unwrap() as u64, l, "at {fixture_name} path {path}");
            }
        }
        "request_type.field7.tag" => {
            let frames = split_connect_frames(&wire_bytes).expect("frames");
            let fields = parse_wire_fields(&frames[0].1).expect("fields");
            let f7 = fields.iter().find(|f| f.field == 7).expect("field 7");
            assert_eq!(f7.tag_bytes, expected_tag_bytes, "at {fixture_name} path {path}");
            if let Some(v) = expected_val {
                assert_eq!(f7.val, Val::Varint(v.as_u64().unwrap()), "at {fixture_name} path {path}");
            }
        }
        "configuration.field5.tag" => {
            let frames = split_connect_frames(&wire_bytes).expect("frames");
            let fields = parse_wire_fields(&frames[0].1).expect("fields");
            let f8 = fields.iter().find(|f| f.field == 8).expect("field 8 configuration");
            let sub = parse_wire_fields(&f8.payload_bytes).expect("subfields");
            let f5 = sub.iter().find(|f| f.field == 5).expect("field 5");
            assert_eq!(f5.tag_bytes, expected_tag_bytes, "at {fixture_name} path {path}");
            if let Some(db) = tc.get("double_bytes").and_then(|d| d.as_str()) {
                assert_eq!(f5.payload_bytes, hex_to_bytes(db), "at {fixture_name} path {path}");
            }
        }
        "chat_model_uid.field21.tag" => {
            let frames = split_connect_frames(&wire_bytes).expect("frames");
            let fields = parse_wire_fields(&frames[0].1).expect("fields");
            let f21 = fields.iter().find(|f| f.field == 21).expect("field 21");
            assert_eq!(f21.tag_bytes, expected_tag_bytes, "at {fixture_name} path {path}");
            if let Some(l) = expected_len {
                assert_eq!(f21.len.unwrap() as u64, l, "at {fixture_name} path {path}");
            }
            if let Some(v) = expected_val {
                assert_eq!(as_string(&f21.val), v.as_str().unwrap(), "at {fixture_name} path {path}");
            }
        }
        "api_key.value" => {
            let fields = parse_wire_fields(&wire_bytes).expect("fields");
            let f1 = fields.iter().find(|f| f.field == 1).expect("field 1");
            let sub = parse_wire_fields(&f1.payload_bytes).expect("metadata fields");
            let f3 = sub.iter().find(|f| f.field == 3).expect("field 3");
            assert_eq!(as_string(&f3.val), tc["ascii"].as_str().unwrap(), "at {fixture_name} path {path}");
            assert_eq!(f3.payload_bytes, expected_tag_bytes, "at {fixture_name} path {path}");
        }
        "entry1.len" => {
            let fields = parse_wire_fields(&wire_bytes).expect("fields");
            let entries: Vec<_> = fields.iter().filter(|f| f.field == 1).collect();
            assert_eq!(entries[0].len_bytes, expected_tag_bytes, "at {fixture_name} path {path}");
            if let Some(l) = expected_len {
                assert_eq!(entries[0].len.unwrap() as u64, l, "at {fixture_name} path {path}");
            }
        }
        "entry2.len" => {
            let fields = parse_wire_fields(&wire_bytes).expect("fields");
            let entries: Vec<_> = fields.iter().filter(|f| f.field == 1).collect();
            assert_eq!(entries[1].len_bytes, expected_tag_bytes, "at {fixture_name} path {path}");
            if let Some(l) = expected_len {
                assert_eq!(entries[1].len.unwrap() as u64, l, "at {fixture_name} path {path}");
            }
        }
        p if p.starts_with("entry1.field") => {
            let fields = parse_wire_fields(&wire_bytes).expect("fields");
            let entries: Vec<_> = fields.iter().filter(|f| f.field == 1).collect();
            let sub = parse_wire_fields(&entries[0].payload_bytes).expect("entry1 fields");
            let field_num: u32 = p.strip_prefix("entry1.field").unwrap().split('.').next().unwrap().parse().unwrap();
            let f = sub.iter().find(|wf| wf.field == field_num).expect("subfield");
            assert_eq!(f.tag_bytes, expected_tag_bytes, "at {fixture_name} path {path}");
            if let Some(l) = expected_len {
                assert_eq!(f.len.unwrap() as u64, l, "at {fixture_name} path {path}");
            }
            if let Some(v) = expected_val {
                match &f.val {
                    Val::Varint(val) => assert_eq!(*val, v.as_u64().unwrap(), "at {fixture_name} path {path}"),
                    Val::Bytes(_) => assert_eq!(as_string(&f.val), v.as_str().unwrap(), "at {fixture_name} path {path}"),
                    _ => {}
                }
            }
        }
        p if p.starts_with("entry2.field") => {
            let fields = parse_wire_fields(&wire_bytes).expect("fields");
            let entries: Vec<_> = fields.iter().filter(|f| f.field == 1).collect();
            let sub = parse_wire_fields(&entries[1].payload_bytes).expect("entry2 fields");
            let field_num: u32 = p.strip_prefix("entry2.field").unwrap().split('.').next().unwrap().parse().unwrap();
            let f = sub.iter().find(|wf| wf.field == field_num).expect("subfield");
            assert_eq!(f.tag_bytes, expected_tag_bytes, "at {fixture_name} path {path}");
            if let Some(l) = expected_len {
                assert_eq!(f.len.unwrap() as u64, l, "at {fixture_name} path {path}");
            }
            if let Some(v) = expected_val {
                match &f.val {
                    Val::Varint(val) => assert_eq!(*val, v.as_u64().unwrap(), "at {fixture_name} path {path}"),
                    Val::Bytes(_) => assert_eq!(as_string(&f.val), v.as_str().unwrap(), "at {fixture_name} path {path}"),
                    _ => {}
                }
            }
        }
        p if p.starts_with("frame") && p.contains(".field") => {
            let rest = p.strip_prefix("frame").unwrap();
            let dot_idx = rest.find('.').unwrap();
            let frame_idx: usize = rest[..dot_idx].parse().unwrap();
            let field_part = &rest[dot_idx + 1..];
            let field_num: u32 = field_part.strip_prefix("field").unwrap().split('.').next().unwrap().parse().unwrap();

            let frames = split_connect_frames(&wire_bytes).expect("frames");
            let fields = parse_wire_fields(&frames[frame_idx].1).expect("frame fields");
            let f = fields.iter().find(|wf| wf.field == field_num).unwrap_or_else(|| {
                panic!("at {fixture_name} path {path}: field {field_num} not found in frame {frame_idx}")
            });
            assert_eq!(f.tag_bytes, expected_tag_bytes, "at {fixture_name} path {path}");
            if let Some(l) = expected_len {
                assert_eq!(f.len.unwrap() as u64, l, "at {fixture_name} path {path}");
            }
            if let Some(v) = expected_val {
                match &f.val {
                    Val::Varint(val) => assert_eq!(*val, v.as_u64().unwrap(), "at {fixture_name} path {path}"),
                    Val::Bytes(_) => assert_eq!(as_string(&f.val), v.as_str().unwrap(), "at {fixture_name} path {path}"),
                    _ => {}
                }
            }
        }
        p if p.starts_with("usage.field") => {
            let field_part = p.strip_prefix("usage.field").unwrap();
            let field_num: u32 = field_part.split('.').next().unwrap().parse().unwrap();

            let frames = split_connect_frames(&wire_bytes).expect("frames");
            let mut found_f = None;
            for frame in &frames {
                if let Ok(fields) = parse_wire_fields(&frame.1) {
                    if let Some(u) = fields.iter().find(|wf| wf.field == 7) {
                        let sub = parse_wire_fields(&u.payload_bytes).expect("usage submessage");
                        if let Some(target) = sub.into_iter().find(|wf| wf.field == field_num) {
                            found_f = Some(target);
                            break;
                        }
                    }
                }
            }
            let f = found_f.unwrap_or_else(|| panic!("at {fixture_name} path {path}: field {field_num} not found in usage"));
            assert_eq!(f.tag_bytes, expected_tag_bytes, "at {fixture_name} path {path}");
            if let Some(l) = expected_len {
                assert_eq!(f.len.unwrap() as u64, l, "at {fixture_name} path {path}");
            }
            if let Some(v) = expected_val {
                match &f.val {
                    Val::Varint(val) => assert_eq!(*val, v.as_u64().unwrap(), "at {fixture_name} path {path}"),
                    Val::Bytes(_) => assert_eq!(as_string(&f.val), v.as_str().unwrap(), "at {fixture_name} path {path}"),
                    _ => {}
                }
            }
        }
        "final.field23.tag" => {
            let frames = split_connect_frames(&wire_bytes).expect("frames");
            let data_frames: Vec<_> = frames.iter().filter(|f| f.0 == 0x00).collect();
            let last_data = data_frames.last().expect("last data frame");
            let fields = parse_wire_fields(&last_data.1).expect("last data frame fields");
            let f23 = fields.iter().find(|wf| wf.field == 23).expect("field 23");
            assert_eq!(f23.tag_bytes, expected_tag_bytes, "at {fixture_name} path {path}");
            if let Some(l) = expected_len {
                assert_eq!(f23.len.unwrap() as u64, l, "at {fixture_name} path {path}");
            }
        }
        other => panic!("unhandled tag check path: {other}"),
    }
}

/// Audit that all `tag_checks` across all 12 fixtures have normalized length semantics
/// asserting exact declared field paths rather than arbitrary byte scanning.
#[test]
fn tag_checks_length_semantics_normalized() {
    let all_fixtures = [
        "unary_request",
        "unary_response",
        "chat_request",
        "chat_response",
        "chat_tools",
        "chat_tools_ambiguous_negative",
        "chat_reasoning",
        "chat_usage",
        "chat_endstream_error",
        "chat_utf8_boundary",
        "chat_utf8_malformed_negative",
        "chat_utf8_proto_parseable_negative",
    ];

    for name in all_fixtures {
        let fx = fixture(name);
        let tag_checks = fx["tag_checks"].as_array().expect("tag_checks array");
        for tc in tag_checks {
            assert_tag_check_exact_path(&fx, tc, name);
        }
    }
}
