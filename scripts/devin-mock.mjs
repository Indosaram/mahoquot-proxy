/**
 * Devin Upstream Mock Harness for mahoquot-proxy.
 *
 * Implements an independent mock server for Devin's Connect RPC endpoints:
 * - Unary: POST /exa.api_server_pb.ApiServerService/GetCascadeModelConfigs (application/proto)
 * - Streaming: POST /exa.api_server_pb.ApiServerService/GetChatMessage (application/connect+proto)
 *
 * Provides control endpoints under /__control for test scenarios, event-driven
 * connection-close assertions, and sanitized request capture inspections.
 *
 * Built with Bun 1.4 builtins only. Zero npm dependencies.
 */

// ─── 1. Independent Protobuf Wire Primitives ─────────────────────────────────

export function encodeVarint(val) {
  let v = BigInt(val);
  const bytes = [];
  while (v >= 0x80n) {
    bytes.push(Number(v & 0x7fn) | 0x80);
    v >>= 7n;
  }
  bytes.push(Number(v & 0x7fn));
  return new Uint8Array(bytes);
}

export function decodeVarint(buf, offset = 0) {
  let result = 0n;
  let shift = 0n;
  let pos = offset;
  while (pos < buf.length) {
    const b = buf[pos++];
    result |= BigInt(b & 0x7f) << shift;
    if ((b & 0x80) === 0) {
      return { value: result, length: pos - offset };
    }
    shift += 7n;
    if (shift > 63n) throw new Error("Varint overflow");
  }
  throw new Error("Unexpected EOF reading varint");
}

export function concatBytes(arrays) {
  const total = arrays.reduce((acc, a) => acc + a.length, 0);
  const out = new Uint8Array(total);
  let off = 0;
  for (const a of arrays) {
    out.set(a, off);
    off += a.length;
  }
  return out;
}

export function encodeFieldTag(fieldNumber, wireType) {
  return encodeVarint((fieldNumber << 3) | wireType);
}

export function encodeVarintField(fieldNumber, val) {
  return concatBytes([encodeFieldTag(fieldNumber, 0), encodeVarint(val)]);
}

export function encodeStringField(fieldNumber, str) {
  const bytes = new TextEncoder().encode(str);
  return concatBytes([encodeFieldTag(fieldNumber, 2), encodeVarint(bytes.length), bytes]);
}

export function encodeBytesField(fieldNumber, bytes) {
  return concatBytes([encodeFieldTag(fieldNumber, 2), encodeVarint(bytes.length), bytes]);
}

export function encodeDoubleField(fieldNumber, floatVal) {
  const buf = new Uint8Array(8);
  new DataView(buf.buffer).setFloat64(0, floatVal, true);
  return concatBytes([encodeFieldTag(fieldNumber, 1), buf]);
}

export function encodeFloatField(fieldNumber, floatVal) {
  const buf = new Uint8Array(4);
  new DataView(buf.buffer).setFloat32(0, floatVal, true);
  return concatBytes([encodeFieldTag(fieldNumber, 5), buf]);
}

export function parseFields(buf) {
  const fields = [];
  let pos = 0;
  while (pos < buf.length) {
    const { value: tag, length: tagLen } = decodeVarint(buf, pos);
    pos += tagLen;
    const fieldNumber = Number(tag >> 3n);
    const wireType = Number(tag & 7n);
    if (wireType === 0) {
      const { value: val, length: valLen } = decodeVarint(buf, pos);
      pos += valLen;
      fields.push({ fieldNumber, wireType, value: val });
    } else if (wireType === 2) {
      const { value: lenBig, length: lenLen } = decodeVarint(buf, pos);
      pos += lenLen;
      const length = Number(lenBig);
      const data = buf.subarray(pos, pos + length);
      pos += length;
      fields.push({ fieldNumber, wireType, data });
    } else if (wireType === 1) {
      const data = buf.subarray(pos, pos + 8);
      pos += 8;
      fields.push({ fieldNumber, wireType, data });
    } else if (wireType === 5) {
      const data = buf.subarray(pos, pos + 4);
      pos += 4;
      fields.push({ fieldNumber, wireType, data });
    } else {
      throw new Error(`Unsupported wire type ${wireType} at pos ${pos}`);
    }
  }
  return fields;
}

export function findField(fields, fieldNumber) {
  return fields.find((f) => f.fieldNumber === fieldNumber);
}

export function findSubfields(fields, fieldNumber) {
  const field = findField(fields, fieldNumber);
  if (!field || !field.data) return [];
  return parseFields(field.data);
}

// ─── 2. Connect Envelope Framing Primitives ──────────────────────────────────

export function encodeFrame(flag, payloadBytes) {
  const header = new Uint8Array(5);
  header[0] = flag;
  new DataView(header.buffer).setUint32(1, payloadBytes.length, false);
  const combined = new Uint8Array(5 + payloadBytes.length);
  combined.set(header, 0);
  combined.set(payloadBytes, 5);
  return combined;
}

export function encodeDataFrame(payloadBytes) {
  return encodeFrame(0x00, payloadBytes);
}

export function encodeEndStreamFrame(errorObj = null) {
  const payloadJson = errorObj ? { error: errorObj } : { error: null };
  const payloadBytes = new TextEncoder().encode(JSON.stringify(payloadJson));
  return encodeFrame(0x02, payloadBytes);
}

export function parseConnectFrames(buffer) {
  const frames = [];
  let offset = 0;
  while (offset + 5 <= buffer.length) {
    const flag = buffer[offset];
    const length = new DataView(buffer.buffer, buffer.byteOffset + offset).getUint32(1, false);
    if (offset + 5 + length > buffer.length) {
      break;
    }
    const payload = buffer.subarray(offset + 5, offset + 5 + length);
    frames.push({ flag, length, payload });
    offset += 5 + length;
  }
  return { frames, consumed: offset };
}

// ─── 3. Devin Domain Model Encoders ──────────────────────────────────────────

export function encodeClientModelConfig({
  label,
  disabled,
  supports_images,
  is_premium,
  max_tokens,
  model_uid,
}) {
  const parts = [];
  if (label) parts.push(encodeStringField(1, label));
  if (disabled) parts.push(encodeVarintField(4, 1));
  if (supports_images) parts.push(encodeVarintField(5, 1));
  if (is_premium) parts.push(encodeVarintField(7, 1));
  if (max_tokens !== undefined) parts.push(encodeVarintField(18, max_tokens));
  if (model_uid) parts.push(encodeStringField(22, model_uid));
  const inner = concatBytes(parts);
  return encodeBytesField(1, inner); // repeated field 1 in GetCascadeModelConfigsResponse
}

export function encodeModelConfigsResponse(configs) {
  const encoded = configs.map(encodeClientModelConfig);
  return concatBytes(encoded);
}

export function encodeChatMessageResponse({
  message_id = "resp-0001",
  delta_text,
  delta_thinking,
  delta_signature,
  thinking_redacted,
  stop_reason,
  actual_model_uid,
  delta_tool_calls,
  usage,
}) {
  const parts = [];
  if (message_id) parts.push(encodeStringField(1, message_id));
  if (delta_text !== undefined) parts.push(encodeStringField(3, delta_text));
  if (stop_reason !== undefined) parts.push(encodeVarintField(5, stop_reason));

  if (delta_tool_calls && delta_tool_calls.length > 0) {
    for (const tc of delta_tool_calls) {
      const tcParts = [];
      if (tc.id) tcParts.push(encodeStringField(1, tc.id));
      if (tc.name) tcParts.push(encodeStringField(2, tc.name));
      if (tc.arguments_json !== undefined) tcParts.push(encodeStringField(3, tc.arguments_json));
      parts.push(encodeBytesField(6, concatBytes(tcParts)));
    }
  }

  if (usage) {
    const uParts = [];
    if (usage.input_tokens !== undefined) uParts.push(encodeVarintField(2, usage.input_tokens));
    if (usage.output_tokens !== undefined) uParts.push(encodeVarintField(3, usage.output_tokens));
    if (usage.cache_write_tokens !== undefined) uParts.push(encodeVarintField(4, usage.cache_write_tokens));
    if (usage.cache_read_tokens !== undefined) uParts.push(encodeVarintField(5, usage.cache_read_tokens));
    if (usage.model_uid) uParts.push(encodeStringField(9, usage.model_uid));
    parts.push(encodeBytesField(7, concatBytes(uParts)));
  }

  if (delta_thinking !== undefined) parts.push(encodeStringField(9, delta_thinking));
  if (delta_signature !== undefined) parts.push(encodeStringField(10, delta_signature));
  if (thinking_redacted !== undefined) parts.push(encodeVarintField(11, thinking_redacted ? 1 : 0));
  if (actual_model_uid) parts.push(encodeStringField(23, actual_model_uid));

  return concatBytes(parts);
}

// ─── 4. Client Request Wire Builders (For Verification & Tests) ───────────────

export function buildGetCascadeModelConfigsRequestWire(apiKey) {
  const metadataParts = [];
  metadataParts.push(encodeStringField(3, apiKey));
  const metadataBytes = concatBytes(metadataParts);
  return encodeBytesField(1, metadataBytes);
}

export function buildGetChatMessageRequestWire({
  token,
  modelUid,
  prompt = "",
  messages = [],
  tools = [],
}) {
  const parts = [];

  // field 1: metadata
  const metaParts = [];
  metaParts.push(encodeStringField(1, "chisel"));
  metaParts.push(encodeStringField(2, "3000.2.17"));
  metaParts.push(encodeStringField(3, token));
  metaParts.push(encodeStringField(4, "en"));
  metaParts.push(encodeStringField(5, "win"));
  parts.push(encodeBytesField(1, concatBytes(metaParts)));

  // field 2: prompt
  if (prompt) {
    parts.push(encodeStringField(2, prompt));
  }

  // field 3: chat_message_prompts
  for (let i = 0; i < messages.length; i++) {
    const msg = messages[i];
    const msgParts = [];
    msgParts.push(encodeStringField(1, `msg-${i + 1}`));
    const sourceVal = msg.role === "user" ? 1 : msg.role === "assistant" ? 2 : msg.role === "tool" ? 4 : 0;
    msgParts.push(encodeVarintField(2, sourceVal));
    if (msg.content) msgParts.push(encodeStringField(3, msg.content));
    if (msg.toolCallId) msgParts.push(encodeStringField(7, msg.toolCallId));
    if (msg.toolCalls && msg.toolCalls.length > 0) {
      for (const tc of msg.toolCalls) {
        const tcParts = [];
        if (tc.id) tcParts.push(encodeStringField(1, tc.id));
        if (tc.name) tcParts.push(encodeStringField(2, tc.name));
        if (tc.args !== undefined) tcParts.push(encodeStringField(3, tc.args));
        msgParts.push(encodeBytesField(6, concatBytes(tcParts)));
      }
    }
    parts.push(encodeBytesField(3, concatBytes(msgParts)));
  }

  // field 7: request_type = 5 (CASCADE)
  parts.push(encodeVarintField(7, 5));

  // field 10: tools
  for (const t of tools) {
    const toolParts = [];
    if (t.name) toolParts.push(encodeStringField(1, t.name));
    if (t.description) toolParts.push(encodeStringField(2, t.description));
    if (t.json_schema_string) toolParts.push(encodeStringField(3, t.json_schema_string));
    parts.push(encodeBytesField(10, concatBytes(toolParts)));
  }

  // field 21: chat_model_uid
  if (modelUid) {
    parts.push(encodeStringField(21, modelUid));
  }

  return concatBytes(parts);
}

// ─── 5. Mock Server State & Synthetic Accounts ───────────────────────────────

const SYNTHETIC_MODELS = {
  glm52: {
    label: "GLM-5.2",
    supports_images: true,
    is_premium: true,
    max_tokens: 200000,
    model_uid: "glm-5-2",
  },
  swe17: {
    label: "SWE-1.7",
    supports_images: true,
    is_premium: false,
    max_tokens: 262000,
    model_uid: "swe-1-7",
  },
};

class EventBus {
  listeners = new Set();

  emit(event) {
    for (const listener of this.listeners) {
      try {
        listener(event);
      } catch (err) {
        // ignore listener error
      }
    }
  }

  waitFor(predicate, timeoutMs = 5000) {
    return new Promise((resolve, reject) => {
      let timer = null;
      const check = (event) => {
        if (predicate(event)) {
          if (timer) clearTimeout(timer);
          this.listeners.delete(check);
          resolve(event);
        }
      };
      timer = setTimeout(() => {
        this.listeners.delete(check);
        reject(new Error(`Timed out after ${timeoutMs}ms waiting for event`));
      }, timeoutMs);
      this.listeners.add(check);
    });
  }
}

class GateBarrier {
  released = false;
  waiters = [];

  wait(abortSignal) {
    if (this.released) return Promise.resolve("released");
    return new Promise((resolve) => {
      const onAbort = () => {
        const idx = this.waiters.indexOf(done);
        if (idx !== -1) this.waiters.splice(idx, 1);
        resolve("aborted");
      };
      const done = (reason) => {
        if (abortSignal) abortSignal.removeEventListener("abort", onAbort);
        resolve(reason);
      };
      if (abortSignal) {
        if (abortSignal.aborted) return resolve("aborted");
        abortSignal.addEventListener("abort", onAbort, { once: true });
      }
      this.waiters.push(done);
    });
  }

  release() {
    this.released = true;
    const current = this.waiters.splice(0);
    for (const waiter of current) waiter("released");
    return current.length;
  }

  reset() {
    this.released = false;
    this.waiters = [];
  }
}

export class DevinMockState {
  scenario = "default";
  scenarioOptions = {};
  requestCount = 0;
  connectionCloseCount = 0;
  captures = [];
  events = [];
  bus = new EventBus();
  gate = new GateBarrier();

  recordEvent(type, data = {}) {
    const event = { type, timestamp: Date.now(), ...data };
    this.events.push(event);
    this.bus.emit(event);
    return event;
  }

  recordCapture(capture) {
    this.requestCount++;
    this.captures.push(capture);
  }

  reset() {
    this.scenario = "default";
    this.scenarioOptions = {};
    this.requestCount = 0;
    this.connectionCloseCount = 0;
    this.captures = [];
    this.events = [];
    this.gate.reset();
  }
}

// ─── 6. Request Authentication & Field Verification Helper ────────────────────

export function redactScenarioOptions(options) {
  if (!options || typeof options !== "object") return {};
  const out = {};
  for (const [k, v] of Object.entries(options)) {
    if (k === "account_a_token" || k === "account_b_token" || k.toLowerCase().includes("token")) {
      out[k] = "[REDACTED]";
    } else {
      out[k] = v;
    }
  }
  return out;
}

function extractAuthToken(authHeader) {
  if (!authHeader || !authHeader.startsWith("Basic ")) {
    return null;
  }
  const rest = authHeader.slice(6).trim();
  const L = rest.length;
  // Must be <token>-<token> where len(T) >= 1, so L >= 3 and L must be odd (2*k + 1)
  if (L < 3 || L % 2 === 0) {
    return null;
  }
  const half = (L - 1) / 2;
  if (rest[half] !== "-") {
    return null;
  }
  const tokenA = rest.slice(0, half);
  const tokenB = rest.slice(half + 1);
  if (tokenA !== tokenB) {
    return null;
  }
  return tokenA;
}

// ─── 7. Mock Server Handler ───────────────────────────────────────────────────

export function createMockServer({
  port = 0,
  host = "127.0.0.1",
  initialScenario = "default",
  initialScenarioOptions = {},
} = {}) {
  const state = new DevinMockState();
  state.scenario = initialScenario;
  state.scenarioOptions = initialScenarioOptions;

  let bunServer = null;

  const app = {
    state,
    get port() {
      return bunServer?.port ?? 0;
    },
    get host() {
      return host;
    },
    get url() {
      return `http://${host}:${this.port}`;
    },
    stop(closeActive = true) {
      if (bunServer) {
        bunServer.stop(closeActive);
        bunServer = null;
      }
    },
    async fetch(req) {
      const url = new URL(req.url);
      const pathname = url.pathname;

      // ── Control Endpoints ──
      if (pathname.startsWith("/__control/")) {
        if (pathname === "/__control/state" && req.method === "GET") {
          return new Response(
            JSON.stringify({
              scenario: state.scenario,
              scenario_options: redactScenarioOptions(state.scenarioOptions),
              request_count: state.requestCount,
              connection_close_count: state.connectionCloseCount,
              captures: state.captures,
              events: state.events,
            }),
            { headers: { "Content-Type": "application/json" } }
          );
        }

        if (pathname === "/__control/scenario" && req.method === "POST") {
          const body = await req.json().catch(() => ({}));
          state.scenario = body.scenario || "default";
          state.scenarioOptions = body.options || {};
          return new Response(
            JSON.stringify({ ok: true, scenario: state.scenario }),
            { headers: { "Content-Type": "application/json" } }
          );
        }

        if (pathname === "/__control/reset" && req.method === "POST") {
          state.reset();
          return new Response(JSON.stringify({ ok: true }), {
            headers: { "Content-Type": "application/json" },
          });
        }

        if (pathname === "/__control/gate/release" && req.method === "POST") {
          const released = state.gate.release();
          return new Response(JSON.stringify({ ok: true, released }), {
            headers: { "Content-Type": "application/json" },
          });
        }

        if (pathname === "/__control/wait-event" && req.method === "POST") {
          const { type, timeout_ms = 5000, since_timestamp = 0 } = await req.json().catch(() => ({}));
          const existing = state.events.slice().reverse().find((e) => e.type === type && e.timestamp >= since_timestamp);
          if (existing) {
            return new Response(JSON.stringify({ ok: true, event: existing }), {
              headers: { "Content-Type": "application/json" },
            });
          }
          try {
            const event = await state.bus.waitFor((e) => e.type === type && e.timestamp >= since_timestamp, timeout_ms);
            return new Response(JSON.stringify({ ok: true, event }), {
              headers: { "Content-Type": "application/json" },
            });
          } catch (err) {
            return new Response(
              JSON.stringify({ ok: false, error: err.message }),
              { status: 408, headers: { "Content-Type": "application/json" } }
            );
          }
        }

        return new Response("Not found", { status: 404 });
      }

      // Allow scenario override via custom test header
      const activeScenario = req.headers.get("x-mock-scenario") || state.scenario;

      // ── Endpoint 1: GetCascadeModelConfigs (Unary application/proto) ──
      if (pathname === "/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs") {
        state.recordEvent("request_start", { path: pathname, method: "POST" });

        // 1. Validate Content-Type
        const contentType = req.headers.get("content-type");
        if (contentType !== "application/proto") {
          state.recordEvent("validation_error", { error: "invalid_content_type", got: contentType });
          return new Response(
            JSON.stringify({ error: { code: "invalid_argument", message: "expected application/proto" } }),
            { status: 415, headers: { "Content-Type": "application/json" } }
          );
        }

        // 2. Validate Authorization Header
        const authHeader = req.headers.get("authorization");
        const token = extractAuthToken(authHeader);
        if (!token) {
          state.recordEvent("validation_error", { error: "missing_or_invalid_auth_header" });
          return new Response(
            JSON.stringify({ error: { code: "unauthenticated", message: "invalid Authorization header" } }),
            { status: 401, headers: { "Content-Type": "application/json" } }
          );
        }

        // 3. Read and Parse Unframed Protobuf Body
        const bodyBuf = new Uint8Array(await req.arrayBuffer());
        let fields = [];
        try {
          fields = parseFields(bodyBuf);
        } catch (e) {
          return new Response(
            JSON.stringify({ error: { code: "invalid_argument", message: "malformed protobuf body" } }),
            { status: 400, headers: { "Content-Type": "application/json" } }
          );
        }

        // Validate metadata.api_key matching token
        const metaField = findField(fields, 1);
        let bodyApiKey = null;
        if (metaField && metaField.data) {
          const metaSubfields = parseFields(metaField.data);
          const apiKeyField = findField(metaSubfields, 3);
          if (apiKeyField && apiKeyField.data) {
            bodyApiKey = new TextDecoder().decode(apiKeyField.data);
          }
        }

        if (bodyApiKey !== token) {
          state.recordEvent("validation_error", {
            error: "token_mismatch",
            header_token_redacted: "[REDACTED]",
            body_token_redacted: "[REDACTED]",
          });
          return new Response(
            JSON.stringify({ error: { code: "unauthenticated", message: "metadata.api_key does not match authorization header" } }),
            { status: 401, headers: { "Content-Type": "application/json" } }
          );
        }

        // Record Sanitized Capture
        state.recordCapture({
          id: `req_${Date.now()}_${Math.random().toString(36).slice(2, 7)}`,
          method: "POST",
          path: pathname,
          headers: {
            "content-type": contentType,
            authorization: "Basic [REDACTED]-[REDACTED]",
            "connect-protocol-version": req.headers.get("connect-protocol-version") || undefined,
          },
          sanitized_body: {
            api_key: "[REDACTED]",
            raw_bytes_len: bodyBuf.length,
          },
          client_closed_early: false,
          timestamp: Date.now(),
        });

        // Generate response based on activeScenario and account token
        let modelConfigs = [];
        if (activeScenario === "disjoint-catalogs") {
          const optA = state.scenarioOptions.account_a_token;
          const optB = state.scenarioOptions.account_b_token;

          const isAccountA =
            token === "devin-dummy-account-a" ||
            token === "devin-session-token$fixture-a" ||
            (optA && token === optA);

          const isAccountB =
            token === "devin-dummy-account-b" ||
            token === "devin-session-token$fixture-b" ||
            (optB && token === optB);

          if (isAccountA) {
            modelConfigs = [SYNTHETIC_MODELS.glm52];
          } else if (isAccountB) {
            modelConfigs = [SYNTHETIC_MODELS.swe17];
          } else if (token === "dummy-token") {
            modelConfigs = [SYNTHETIC_MODELS.glm52, SYNTHETIC_MODELS.swe17];
          } else {
            modelConfigs = [];
          }
        } else {
          // Default scenario: returns both synthetic models
          modelConfigs = [SYNTHETIC_MODELS.glm52, SYNTHETIC_MODELS.swe17];
        }

        const respBytes = encodeModelConfigsResponse(modelConfigs);
        state.recordEvent("request_end", { path: pathname });

        return new Response(respBytes, {
          status: 200,
          headers: {
            "Content-Type": "application/proto",
            "Connect-Protocol-Version": "1",
          },
        });
      }

      // ── Endpoint 2: GetChatMessage (Streaming application/connect+proto) ──
      if (pathname === "/exa.api_server_pb.ApiServerService/GetChatMessage") {
        state.recordEvent("request_start", { path: pathname, method: "POST" });

        // 1. Validate Content-Type
        const contentType = req.headers.get("content-type");
        if (contentType !== "application/connect+proto") {
          state.recordEvent("validation_error", { error: "invalid_content_type", got: contentType });
          return new Response(
            JSON.stringify({ error: { code: "invalid_argument", message: "expected application/connect+proto" } }),
            { status: 415, headers: { "Content-Type": "application/json" } }
          );
        }

        // 2. Validate Authorization Header
        const authHeader = req.headers.get("authorization");
        const token = extractAuthToken(authHeader);
        if (!token) {
          state.recordEvent("validation_error", { error: "missing_or_invalid_auth_header" });
          return new Response(
            JSON.stringify({ error: { code: "unauthenticated", message: "invalid Authorization header" } }),
            { status: 401, headers: { "Content-Type": "application/json" } }
          );
        }

        // 3. Read and Parse Connect Framed Protobuf Body
        const bodyBuf = new Uint8Array(await req.arrayBuffer());
        const { frames } = parseConnectFrames(bodyBuf);
        if (frames.length === 0 || frames[0].flag !== 0x00) {
          return new Response(
            JSON.stringify({ error: { code: "invalid_argument", message: "missing request data frame" } }),
            { status: 400, headers: { "Content-Type": "application/json" } }
          );
        }

        let fields = [];
        try {
          fields = parseFields(frames[0].payload);
        } catch (e) {
          return new Response(
            JSON.stringify({ error: { code: "invalid_argument", message: "malformed protobuf payload" } }),
            { status: 400, headers: { "Content-Type": "application/json" } }
          );
        }

        // Validate metadata.api_key matching token
        const metaField = findField(fields, 1);
        let bodyApiKey = null;
        if (metaField && metaField.data) {
          const metaSubfields = parseFields(metaField.data);
          const apiKeyField = findField(metaSubfields, 3);
          if (apiKeyField && apiKeyField.data) {
            bodyApiKey = new TextDecoder().decode(apiKeyField.data);
          }
        }

        if (bodyApiKey !== token) {
          state.recordEvent("validation_error", { error: "token_mismatch" });
          return new Response(
            JSON.stringify({ error: { code: "unauthenticated", message: "metadata.api_key mismatch" } }),
            { status: 401, headers: { "Content-Type": "application/json" } }
          );
        }

        // Validate unprefixed model UID (chat_model_uid is field 21)
        const modelUidField = findField(fields, 21);
        let modelUid = "";
        if (modelUidField && modelUidField.data) {
          modelUid = new TextDecoder().decode(modelUidField.data);
        }
        if (modelUid.startsWith("devin/")) {
          state.recordEvent("validation_error", { error: "prefixed_model_uid", modelUid });
          return new Response(
            JSON.stringify({
              error: {
                code: "invalid_argument",
                message: `model UID must not have devin/ prefix: got '${modelUid}'`,
              },
            }),
            { status: 400, headers: { "Content-Type": "application/json" } }
          );
        }

        // Extract metadata for sanitized capture
        const promptField = findField(fields, 2);
        const promptText = promptField && promptField.data ? new TextDecoder().decode(promptField.data) : "";
        const promptMessages = fields.filter((f) => f.fieldNumber === 3);
        const hasTools = fields.some((f) => f.fieldNumber === 10);
        const hasToolResults = promptMessages.some((m) => {
          const mFields = parseFields(m.data);
          const src = findField(mFields, 2);
          const toolCallId = findField(mFields, 7);
          return (src && Number(src.value) === 4) || (toolCallId && toolCallId.data && toolCallId.data.length > 0);
        });

        const captureRecord = {
          id: `req_${Date.now()}_${Math.random().toString(36).slice(2, 7)}`,
          method: "POST",
          path: pathname,
          headers: {
            "content-type": contentType,
            authorization: "Basic [REDACTED]-[REDACTED]",
            "connect-protocol-version": req.headers.get("connect-protocol-version") || undefined,
          },
          sanitized_body: {
            api_key: "[REDACTED]",
            chat_model_uid: modelUid,
            prompt: promptText,
            message_count: promptMessages.length,
            has_tools: hasTools,
            has_tool_results: hasToolResults,
          },
          client_closed_early: false,
          timestamp: Date.now(),
        };
        state.recordCapture(captureRecord);

        // ── Construct Streaming Response According to Scenario ──

        // Scenario 6: Terminal Code-Only Error
        if (activeScenario === "code-only-error") {
          const endFrame = encodeEndStreamFrame({ code: "resource_exhausted" });
          state.recordEvent("request_end", { path: pathname, scenario: activeScenario });
          return new Response(endFrame, {
            status: 200,
            headers: {
              "Content-Type": "application/connect+proto",
              "Connect-Protocol-Version": "1",
            },
          });
        }

        // Scenario 7: Late Terminal Error
        if (activeScenario === "late-error") {
          const f1 = encodeDataFrame(encodeChatMessageResponse({ delta_text: "Processing request..." }));
          const f2 = encodeEndStreamFrame({ code: "unavailable", message: "server disconnected unexpectedly" });
          const combined = concatBytes([f1, f2]);
          state.recordEvent("request_end", { path: pathname, scenario: activeScenario });
          return new Response(combined, {
            status: 200,
            headers: {
              "Content-Type": "application/connect+proto",
              "Connect-Protocol-Version": "1",
            },
          });
        }

        // Scenario 5: Output Limit
        if (activeScenario === "output-limit") {
          const f1 = encodeDataFrame(
            encodeChatMessageResponse({
              delta_text: "Output truncated due to token limit.",
              stop_reason: 3, // STOP_REASON_MAX_TOKENS
            })
          );
          const f2 = encodeEndStreamFrame(null);
          state.recordEvent("request_end", { path: pathname, scenario: activeScenario });
          return new Response(concatBytes([f1, f2]), {
            status: 200,
            headers: {
              "Content-Type": "application/connect+proto",
              "Connect-Protocol-Version": "1",
            },
          });
        }

        // Scenario 3: Usage / Cache Snapshots
        if (activeScenario === "usage") {
          const f1 = encodeDataFrame(encodeChatMessageResponse({ delta_text: "Usage test output." }));
          const f2 = encodeDataFrame(
            encodeChatMessageResponse({
              usage: {
                input_tokens: 12,
                output_tokens: 34,
                cache_write_tokens: 7,
                cache_read_tokens: 5,
                model_uid: modelUid || "glm-5-2",
              },
            })
          );
          const f3 = encodeEndStreamFrame(null);
          state.recordEvent("request_end", { path: pathname, scenario: activeScenario });
          return new Response(concatBytes([f1, f2, f3]), {
            status: 200,
            headers: {
              "Content-Type": "application/connect+proto",
              "Connect-Protocol-Version": "1",
            },
          });
        }

        // Scenario 4: Tools and Tool-Result Final Turn
        if (activeScenario === "tools") {
          if (hasToolResults) {
            // Turn 2: Final answer after tool execution
            const f1 = encodeDataFrame(
              encodeChatMessageResponse({
                delta_text: "The weather in Seoul is 22C and clear.",
                stop_reason: 0,
                actual_model_uid: modelUid || "glm-5-2",
              })
            );
            const f2 = encodeEndStreamFrame(null);
            state.recordEvent("request_end", { path: pathname, turn: 2 });
            return new Response(concatBytes([f1, f2]), {
              status: 200,
              headers: {
                "Content-Type": "application/connect+proto",
                "Connect-Protocol-Version": "1",
              },
            });
          } else {
            // Turn 1: Emits tool call deltas
            const f1 = encodeDataFrame(
              encodeChatMessageResponse({
                delta_tool_calls: [
                  { id: "call_001", name: "get_weather", arguments_json: '{"city":"Seoul"}' },
                ],
                stop_reason: 10, // STOP_REASON_FUNCTION_CALL
                actual_model_uid: modelUid || "glm-5-2",
              })
            );
            const f2 = encodeEndStreamFrame(null);
            state.recordEvent("request_end", { path: pathname, turn: 1 });
            return new Response(concatBytes([f1, f2]), {
              status: 200,
              headers: {
                "Content-Type": "application/connect+proto",
                "Connect-Protocol-Version": "1",
              },
            });
          }
        }

        // Scenario 8: Transport Cancellation & Deliberate Chunks
        if (activeScenario === "transport-cancellation") {
          let clientClosed = false;

          const handleClientClose = (reason) => {
            if (clientClosed) return;
            clientClosed = true;
            captureRecord.client_closed_early = true;
            state.connectionCloseCount++;
            state.recordEvent("client_closed", { path: pathname, reason: String(reason || "aborted") });
          };

          req.signal.addEventListener("abort", () => {
            handleClientClose("req.signal.abort");
          }, { once: true });

          const stream = new ReadableStream({
            async start(controller) {
              // 1. Emit deliberate first chunk (first data frame)
              const firstFrame = encodeDataFrame(
                encodeChatMessageResponse({ delta_text: "Starting stream before pause..." })
              );
              controller.enqueue(firstFrame);

              // 2. Wait at controllable gate barrier or until client aborts
              const waitResult = await state.gate.wait(req.signal);
              if (waitResult === "aborted" || req.signal.aborted) {
                handleClientClose("gate_wait_aborted");
                return;
              }

              // 3. If gate was released, emit second chunk and close
              const secondFrame = encodeDataFrame(
                encodeChatMessageResponse({ delta_text: "Continued after gate release." })
              );
              const endFrame = encodeEndStreamFrame(null);
              controller.enqueue(secondFrame);
              controller.enqueue(endFrame);
              controller.close();
              state.recordEvent("request_end", { path: pathname, scenario: activeScenario });
            },
            cancel(reason) {
              handleClientClose(reason);
            },
          });

          return new Response(stream, {
            status: 200,
            headers: {
              "Content-Type": "application/connect+proto",
              "Connect-Protocol-Version": "1",
            },
          });
        }

        // Default & Normal Scenario: Full Text / Reasoning / Signature / Redacted
        const f1 = encodeDataFrame(encodeChatMessageResponse({ delta_thinking: "Planning the reply." }));
        const f2 = encodeDataFrame(encodeChatMessageResponse({ delta_signature: "sig-opaque-0001" }));
        const f3 = encodeDataFrame(encodeChatMessageResponse({ thinking_redacted: true }));
        const f4 = encodeDataFrame(encodeChatMessageResponse({ delta_text: "Hello, I am Devin." }));
        const f5 = encodeEndStreamFrame(null);
        state.recordEvent("request_end", { path: pathname });

        return new Response(concatBytes([f1, f2, f3, f4, f5]), {
          status: 200,
          headers: {
            "Content-Type": "application/connect+proto",
            "Connect-Protocol-Version": "1",
          },
        });
      }

      return new Response("Not Found", { status: 404 });
    },
  };

  bunServer = Bun.serve({
    port,
    hostname: host,
    fetch: app.fetch.bind(app),
  });

  return app;
}

export async function startMockServer(options = {}) {
  const server = createMockServer(options);
  return server;
}

// ─── 8. CLI Runner ────────────────────────────────────────────────────────────

if (import.meta.main) {
  let port = 0;
  let host = "127.0.0.1";
  let scenario = "default";
  let scenarioOptions = {};

  const args = process.argv.slice(2);
  for (let i = 0; i < args.length; i++) {
    if (args[i] === "--port" && i + 1 < args.length) {
      port = parseInt(args[++i], 10);
    } else if (args[i] === "--host" && i + 1 < args.length) {
      host = args[++i];
    } else if (args[i] === "--scenario" && i + 1 < args.length) {
      scenario = args[++i];
    } else if (args[i] === "--scenario-options" && i + 1 < args.length) {
      try {
        scenarioOptions = JSON.parse(args[++i]);
      } catch (e) {
        // ignore parse error
      }
    }
  }

  const server = await startMockServer({
    port,
    host,
    initialScenario: scenario,
    initialScenarioOptions: scenarioOptions,
  });

  // Print exact single JSON readiness line
  const readiness = JSON.stringify({
    status: "ready",
    port: server.port,
    host: server.host,
    url: server.url,
  });
  process.stdout.write(readiness + "\n");

  const cleanup = () => {
    server.stop(true);
    process.exit(0);
  };

  process.on("SIGINT", cleanup);
  process.on("SIGTERM", cleanup);
}
