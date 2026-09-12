import { describe, it, expect, beforeAll, afterAll } from "bun:test";
import {
  createMockServer,
  startMockServer,
  encodeVarint,
  decodeVarint,
  parseFields,
  findField,
  findSubfields,
  encodeFieldTag,
  encodeVarintField,
  encodeStringField,
  encodeBytesField,
  encodeFrame,
  encodeDataFrame,
  encodeEndStreamFrame,
  parseConnectFrames,
  encodeClientModelConfig,
  encodeModelConfigsResponse,
  encodeChatMessageResponse,
  buildGetChatMessageRequestWire,
  buildGetCascadeModelConfigsRequestWire,
} from "./devin-mock.mjs";

describe("Devin Upstream Mock Harness", () => {
  let server;
  let baseUrl;

  beforeAll(async () => {
    server = await startMockServer({ port: 0, host: "127.0.0.1" });
    baseUrl = `http://127.0.0.1:${server.port}`;
  });

  afterAll(async () => {
    if (server) {
      await server.stop();
    }
  });

  // ─── 1. CLI and Readiness Line ──────────────────────────────────────────

  describe("CLI and process lifecycle", () => {
    it("starts as child process, prints single JSON readiness line with port, and exits on SIGTERM", async () => {
      let proc = null;
      let timeoutTimer = null;
      try {
        proc = Bun.spawn(["bun", "scripts/devin-mock.mjs", "--port", "0"], {
          cwd: process.cwd(),
          stdout: "pipe",
          stderr: "pipe",
        });

        const reader = proc.stdout.getReader();
        const decoder = new TextDecoder();
        let lineBuffer = "";
        let readinessData = null;

        const readDeadline = Date.now() + 5000;
        while (Date.now() < readDeadline && !readinessData) {
          const readPromise = reader.read();
          const timeoutPromise = new Promise((_, reject) => {
            timeoutTimer = setTimeout(() => reject(new Error("Timeout waiting for stdout")), 5000);
          });
          const { value, done } = await Promise.race([readPromise, timeoutPromise]);
          if (timeoutTimer) {
            clearTimeout(timeoutTimer);
            timeoutTimer = null;
          }
          if (done) break;
          lineBuffer += decoder.decode(value, { stream: true });
          const newlineIndex = lineBuffer.indexOf("\n");
          if (newlineIndex !== -1) {
            const firstLine = lineBuffer.slice(0, newlineIndex).trim();
            readinessData = JSON.parse(firstLine);
            break;
          }
        }

        expect(readinessData).not.toBeNull();
        expect(readinessData.status).toBe("ready");
        expect(typeof readinessData.port).toBe("number");
        expect(readinessData.port).toBeGreaterThan(0);
        expect(readinessData.host).toBe("127.0.0.1");
        expect(readinessData.url).toBe(`http://127.0.0.1:${readinessData.port}`);

        // Verify health check on the spawned instance
        const healthRes = await fetch(`${readinessData.url}/__control/state`);
        expect(healthRes.status).toBe(200);

        // Clean shutdown with SIGTERM
        proc.kill(15); // SIGTERM
        const exitCode = await proc.exited;
        expect(exitCode).toBe(0);
      } finally {
        if (timeoutTimer) {
          clearTimeout(timeoutTimer);
          timeoutTimer = null;
        }
        if (proc) {
          try {
            proc.kill(15);
            await proc.exited;
          } catch (_) {
            // Already terminated
          }
        }
      }
    });
  });

  // ─── 2. Wire & Frame Codec Primitives ───────────────────────────────────

  describe("Independent Wire and Connect Framing", () => {
    it("encodes and decodes varints correctly", () => {
      const cases = [0n, 1n, 127n, 128n, 300n, 200000n, 262000n, 0xffffffffn];
      for (const val of cases) {
        const enc = encodeVarint(val);
        const { value, length } = decodeVarint(enc, 0);
        expect(value).toBe(val);
        expect(length).toBe(enc.length);
      }
    });

    it("parses protobuf fields from raw bytes independently", () => {
      const nameBytes = new TextEncoder().encode("test-model");
      const wire = new Uint8Array([
        encodeFieldTag(1, 0)[0], // field 1, varint
        42,
        ...encodeStringField(22, "test-model"), // field 22, length-delimited
      ]);
      const fields = parseFields(wire);
      const f1 = findField(fields, 1);
      expect(f1).toBeDefined();
      expect(f1.value).toBe(42n);

      const f22 = findField(fields, 22);
      expect(f22).toBeDefined();
      expect(new TextDecoder().decode(f22.data)).toBe("test-model");
    });

    it("encodes and parses Connect envelope frames (data & end_stream)", () => {
      const dataPayload = new Uint8Array([10, 20, 30, 40]);
      const dataFrame = encodeDataFrame(dataPayload);
      expect(dataFrame[0]).toBe(0x00); // flag
      const dataLen = new DataView(dataFrame.buffer, dataFrame.byteOffset).getUint32(1, false);
      expect(dataLen).toBe(4);

      const endPayload = { error: { code: "resource_exhausted" } };
      const endFrame = encodeEndStreamFrame(endPayload.error);
      expect(endFrame[0]).toBe(0x02); // end_stream flag

      const combined = new Uint8Array(dataFrame.length + endFrame.length);
      combined.set(dataFrame, 0);
      combined.set(endFrame, dataFrame.length);

      const { frames, consumed } = parseConnectFrames(combined);
      expect(frames.length).toBe(2);
      expect(frames[0].flag).toBe(0x00);
      expect(frames[0].payload).toEqual(dataPayload);
      expect(frames[1].flag).toBe(0x02);
      const parsedEndJson = JSON.parse(new TextDecoder().decode(frames[1].payload));
      expect(parsedEndJson.error.code).toBe("resource_exhausted");
      expect(consumed).toBe(combined.length);
    });
  });

  // ─── 3. Authentication & Wire Metadata Validation ───────────────────────

  describe("Authentication and wire field validation", () => {
    beforeAll(async () => {
      await fetch(`${baseUrl}/__control/reset`, { method: "POST" });
    });

    it("rejects request missing Authorization header with HTTP 401", async () => {
      const body = buildGetCascadeModelConfigsRequestWire("dummy-token");
      const res = await fetch(`${baseUrl}/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs`, {
        method: "POST",
        headers: { "Content-Type": "application/proto" },
        body,
      });
      expect(res.status).toBe(401);
    });

    it("rejects request with malformed Authorization (not Basic <token>-<token>)", async () => {
      const body = buildGetCascadeModelConfigsRequestWire("dummy-token");
      const res = await fetch(`${baseUrl}/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs`, {
        method: "POST",
        headers: {
          "Content-Type": "application/proto",
          Authorization: "Bearer dummy-token",
        },
        body,
      });
      expect(res.status).toBe(401);
    });

    it("rejects request where Authorization header token does not match protobuf metadata.api_key", async () => {
      // Header has token-A, body has token-B
      const body = buildGetCascadeModelConfigsRequestWire("token-b");
      const res = await fetch(`${baseUrl}/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs`, {
        method: "POST",
        headers: {
          "Content-Type": "application/proto",
          Authorization: "Basic token-a-token-a",
        },
        body,
      });
      expect(res.status).toBe(401);
    });

    it("rejects request with wrong Content-Type with HTTP 415", async () => {
      const body = buildGetCascadeModelConfigsRequestWire("dummy-token");
      const res = await fetch(`${baseUrl}/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          Authorization: "Basic dummy-token-dummy-token",
        },
        body,
      });
      expect(res.status).toBe(415);
    });

    it("rejects GetChatMessage when Content-Type is not application/connect+proto", async () => {
      const reqWire = buildGetChatMessageRequestWire({
        token: "dummy-token",
        modelUid: "glm-5-2",
        prompt: "hello",
      });
      const framedReq = encodeDataFrame(reqWire);
      const res = await fetch(`${baseUrl}/exa.api_server_pb.ApiServerService/GetChatMessage`, {
        method: "POST",
        headers: {
          "Content-Type": "application/proto", // wrong! should be application/connect+proto
          Authorization: "Basic dummy-token-dummy-token",
        },
        body: framedReq,
      });
      expect(res.status).toBe(415);
    });

    it("rejects GetChatMessage when chat_model_uid has devin/ prefix", async () => {
      const reqWire = buildGetChatMessageRequestWire({
        token: "dummy-token",
        modelUid: "devin/glm-5-2", // Forbidden! Must be unprefixed
        prompt: "hello",
      });
      const framedReq = encodeDataFrame(reqWire);
      const res = await fetch(`${baseUrl}/exa.api_server_pb.ApiServerService/GetChatMessage`, {
        method: "POST",
        headers: {
          "Content-Type": "application/connect+proto",
          Authorization: "Basic dummy-token-dummy-token",
        },
        body: framedReq,
      });
      expect(res.status).toBe(400);
      const json = await res.json();
      expect(json.error.message).toContain("devin/");
    });
  });

  // ─── 4. Scenario 1: Disjoint Per-Account Catalogs ───────────────────────

  describe("Scenario 1: Disjoint Per-Account Catalogs", () => {
    beforeAll(async () => {
      await fetch(`${baseUrl}/__control/reset`, { method: "POST" });
      await fetch(`${baseUrl}/__control/scenario`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ scenario: "disjoint-catalogs" }),
      });
    });

    it("returns account A catalog (glm-5-2 only) for account A credentials", async () => {
      const token = "devin-dummy-account-a";
      const body = buildGetCascadeModelConfigsRequestWire(token);
      const res = await fetch(`${baseUrl}/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs`, {
        method: "POST",
        headers: {
          "Content-Type": "application/proto",
          Authorization: `Basic ${token}-${token}`,
        },
        body,
      });
      expect(res.status).toBe(200);
      expect(res.headers.get("Content-Type")).toBe("application/proto");
      expect(res.headers.get("Connect-Protocol-Version")).toBe("1");

      const resBuf = new Uint8Array(await res.arrayBuffer());
      const fields = parseFields(resBuf);
      // ClientModelConfig is repeated field 1
      const configs = fields.filter((f) => f.fieldNumber === 1);
      expect(configs.length).toBe(1);

      const configFields = parseFields(configs[0].data);
      const modelUidField = findField(configFields, 22);
      expect(new TextDecoder().decode(modelUidField.data)).toBe("glm-5-2");
    });

    it("returns account B catalog (swe-1-7 only) for account B credentials", async () => {
      const token = "devin-dummy-account-b";
      const body = buildGetCascadeModelConfigsRequestWire(token);
      const res = await fetch(`${baseUrl}/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs`, {
        method: "POST",
        headers: {
          "Content-Type": "application/proto",
          Authorization: `Basic ${token}-${token}`,
        },
        body,
      });
      expect(res.status).toBe(200);

      const resBuf = new Uint8Array(await res.arrayBuffer());
      const fields = parseFields(resBuf);
      const configs = fields.filter((f) => f.fieldNumber === 1);
      expect(configs.length).toBe(1);

      const configFields = parseFields(configs[0].data);
      const modelUidField = findField(configFields, 22);
      expect(new TextDecoder().decode(modelUidField.data)).toBe("swe-1-7");
    });

    it("returns account A catalog for devin-session-token$fixture-a", async () => {
      const token = "devin-session-token$fixture-a";
      const body = buildGetCascadeModelConfigsRequestWire(token);
      const res = await fetch(`${baseUrl}/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs`, {
        method: "POST",
        headers: {
          "Content-Type": "application/proto",
          Authorization: `Basic ${token}-${token}`,
        },
        body,
      });
      expect(res.status).toBe(200);

      const resBuf = new Uint8Array(await res.arrayBuffer());
      const fields = parseFields(resBuf);
      const configs = fields.filter((f) => f.fieldNumber === 1);
      expect(configs.length).toBe(1);

      const configFields = parseFields(configs[0].data);
      const modelUidField = findField(configFields, 22);
      expect(new TextDecoder().decode(modelUidField.data)).toBe("glm-5-2");
    });

    it("returns account B catalog for devin-session-token$fixture-b", async () => {
      const token = "devin-session-token$fixture-b";
      const body = buildGetCascadeModelConfigsRequestWire(token);
      const res = await fetch(`${baseUrl}/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs`, {
        method: "POST",
        headers: {
          "Content-Type": "application/proto",
          Authorization: `Basic ${token}-${token}`,
        },
        body,
      });
      expect(res.status).toBe(200);

      const resBuf = new Uint8Array(await res.arrayBuffer());
      const fields = parseFields(resBuf);
      const configs = fields.filter((f) => f.fieldNumber === 1);
      expect(configs.length).toBe(1);

      const configFields = parseFields(configs[0].data);
      const modelUidField = findField(configFields, 22);
      expect(new TextDecoder().decode(modelUidField.data)).toBe("swe-1-7");
    });

    it("supports explicit scenario options for custom disjoint account tokens", async () => {
      const customTokenA = "custom-secret-token-a";
      const customTokenB = "custom-secret-token-b";

      const setRes = await fetch(`${baseUrl}/__control/scenario`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          scenario: "disjoint-catalogs",
          options: {
            account_a_token: customTokenA,
            account_b_token: customTokenB,
          },
        }),
      });
      expect(setRes.status).toBe(200);

      // Verify custom token A returns catalog A
      const resA = await fetch(`${baseUrl}/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs`, {
        method: "POST",
        headers: {
          "Content-Type": "application/proto",
          Authorization: `Basic ${customTokenA}-${customTokenA}`,
        },
        body: buildGetCascadeModelConfigsRequestWire(customTokenA),
      });
      expect(resA.status).toBe(200);
      const configsA = parseFields(new Uint8Array(await resA.arrayBuffer())).filter((f) => f.fieldNumber === 1);
      expect(configsA.length).toBe(1);
      const uidA = findField(parseFields(configsA[0].data), 22);
      expect(new TextDecoder().decode(uidA.data)).toBe("glm-5-2");

      // Verify custom token B returns catalog B
      const resB = await fetch(`${baseUrl}/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs`, {
        method: "POST",
        headers: {
          "Content-Type": "application/proto",
          Authorization: `Basic ${customTokenB}-${customTokenB}`,
        },
        body: buildGetCascadeModelConfigsRequestWire(customTokenB),
      });
      expect(resB.status).toBe(200);
      const configsB = parseFields(new Uint8Array(await resB.arrayBuffer())).filter((f) => f.fieldNumber === 1);
      expect(configsB.length).toBe(1);
      const uidB = findField(parseFields(configsB[0].data), 22);
      expect(new TextDecoder().decode(uidB.data)).toBe("swe-1-7");
    });
  });

  // ─── 5. Scenario 2: Normal Text, Reasoning, Signature, Redacted ─────────

  describe("Scenario 2: Normal Text, Reasoning, Signature, Redacted", () => {
    beforeAll(async () => {
      await fetch(`${baseUrl}/__control/reset`, { method: "POST" });
      await fetch(`${baseUrl}/__control/scenario`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ scenario: "normal" }),
      });
    });

    it("streams thinking delta, signature, redacted flag, text delta, and end stream", async () => {
      const token = "dummy-token";
      const reqWire = buildGetChatMessageRequestWire({
        token,
        modelUid: "glm-5-2",
        prompt: "Ponder and answer.",
      });
      const res = await fetch(`${baseUrl}/exa.api_server_pb.ApiServerService/GetChatMessage`, {
        method: "POST",
        headers: {
          "Content-Type": "application/connect+proto",
          Authorization: `Basic ${token}-${token}`,
        },
        body: encodeDataFrame(reqWire),
      });
      expect(res.status).toBe(200);
      expect(res.headers.get("Content-Type")).toBe("application/connect+proto");

      const buf = new Uint8Array(await res.arrayBuffer());
      const { frames } = parseConnectFrames(buf);
      expect(frames.length).toBeGreaterThanOrEqual(4);

      // Collect elements across frames
      let hasThinking = false;
      let hasSignature = false;
      let hasRedacted = false;
      let hasText = false;
      let hasEndStream = false;

      for (const frame of frames) {
        if (frame.flag === 0x00) {
          const respFields = parseFields(frame.payload);
          if (findField(respFields, 9)) hasThinking = true;
          if (findField(respFields, 10)) hasSignature = true;
          if (findField(respFields, 11)) hasRedacted = true;
          if (findField(respFields, 3)) hasText = true;
        } else if (frame.flag === 0x02) {
          hasEndStream = true;
          const endJson = JSON.parse(new TextDecoder().decode(frame.payload));
          expect(endJson.error).toBeNull();
        }
      }

      expect(hasThinking).toBe(true);
      expect(hasSignature).toBe(true);
      expect(hasRedacted).toBe(true);
      expect(hasText).toBe(true);
      expect(hasEndStream).toBe(true);
    });
  });

  // ─── 6. Scenario 3: Usage / Cache Snapshots ─────────────────────────────

  describe("Scenario 3: Usage and Cache Snapshots", () => {
    beforeAll(async () => {
      await fetch(`${baseUrl}/__control/reset`, { method: "POST" });
      await fetch(`${baseUrl}/__control/scenario`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ scenario: "usage" }),
      });
    });

    it("streams usage stats containing input, output, cache read/write tokens and model_uid", async () => {
      const token = "dummy-token";
      const reqWire = buildGetChatMessageRequestWire({
        token,
        modelUid: "glm-5-2",
        prompt: "Usage check",
      });
      const res = await fetch(`${baseUrl}/exa.api_server_pb.ApiServerService/GetChatMessage`, {
        method: "POST",
        headers: {
          "Content-Type": "application/connect+proto",
          Authorization: `Basic ${token}-${token}`,
        },
        body: encodeDataFrame(reqWire),
      });
      expect(res.status).toBe(200);

      const buf = new Uint8Array(await res.arrayBuffer());
      const { frames } = parseConnectFrames(buf);

      let usageFound = null;
      for (const frame of frames) {
        if (frame.flag === 0x00) {
          const fields = parseFields(frame.payload);
          const usageField = findField(fields, 7); // usage = 7
          if (usageField) {
            const uFields = parseFields(usageField.data);
            usageFound = {
              inputTokens: Number(findField(uFields, 2)?.value ?? 0n),
              outputTokens: Number(findField(uFields, 3)?.value ?? 0n),
              cacheWriteTokens: Number(findField(uFields, 4)?.value ?? 0n),
              cacheReadTokens: Number(findField(uFields, 5)?.value ?? 0n),
              modelUid: new TextDecoder().decode(findField(uFields, 9)?.data ?? new Uint8Array()),
            };
          }
        }
      }

      expect(usageFound).not.toBeNull();
      expect(usageFound.inputTokens).toBe(12);
      expect(usageFound.outputTokens).toBe(34);
      expect(usageFound.cacheWriteTokens).toBe(7);
      expect(usageFound.cacheReadTokens).toBe(5);
      expect(usageFound.modelUid).toBe("glm-5-2");
    });
  });

  // ─── 7. Scenario 4: Tools and Tool-Result Final Turn ─────────────────────

  describe("Scenario 4: Tools and Tool-Result Final Turn", () => {
    beforeAll(async () => {
      await fetch(`${baseUrl}/__control/reset`, { method: "POST" });
      await fetch(`${baseUrl}/__control/scenario`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ scenario: "tools" }),
      });
    });

    it("emits tool call deltas and STOP_REASON_FUNCTION_CALL on turn 1 (no tool result)", async () => {
      const token = "dummy-token";
      const reqWire = buildGetChatMessageRequestWire({
        token,
        modelUid: "glm-5-2",
        prompt: "What is the weather in Seoul?",
        tools: [{ name: "get_weather", description: "Get weather" }],
      });
      const res = await fetch(`${baseUrl}/exa.api_server_pb.ApiServerService/GetChatMessage`, {
        method: "POST",
        headers: {
          "Content-Type": "application/connect+proto",
          Authorization: `Basic ${token}-${token}`,
        },
        body: encodeDataFrame(reqWire),
      });
      expect(res.status).toBe(200);

      const buf = new Uint8Array(await res.arrayBuffer());
      const { frames } = parseConnectFrames(buf);

      const toolCalls = [];
      let stopReason = null;
      for (const f of frames) {
        if (f.flag === 0x00) {
          const fields = parseFields(f.payload);
          const tcFields = fields.filter((x) => x.fieldNumber === 6);
          for (const tc of tcFields) {
            const inner = parseFields(tc.data);
            toolCalls.push({
              id: new TextDecoder().decode(findField(inner, 1)?.data ?? new Uint8Array()),
              name: new TextDecoder().decode(findField(inner, 2)?.data ?? new Uint8Array()),
              args: new TextDecoder().decode(findField(inner, 3)?.data ?? new Uint8Array()),
            });
          }
          const sr = findField(fields, 5);
          if (sr) stopReason = Number(sr.value);
        }
      }

      expect(toolCalls.length).toBeGreaterThanOrEqual(1);
      expect(toolCalls[0].name).toBe("get_weather");
      expect(stopReason).toBe(10); // STOP_REASON_FUNCTION_CALL
    });

    it("emits final turn text on turn 2 (when tool result is present in history)", async () => {
      const token = "dummy-token";
      const reqWire = buildGetChatMessageRequestWire({
        token,
        modelUid: "glm-5-2",
        prompt: "What is the weather?",
        messages: [
          { role: "user", content: "What is the weather?" },
          {
            role: "assistant",
            toolCalls: [{ id: "call_001", name: "get_weather", args: '{"city":"Seoul"}' }],
          },
          { role: "tool", toolCallId: "call_001", content: '{"temp":"22C"}' },
        ],
      });
      const res = await fetch(`${baseUrl}/exa.api_server_pb.ApiServerService/GetChatMessage`, {
        method: "POST",
        headers: {
          "Content-Type": "application/connect+proto",
          Authorization: `Basic ${token}-${token}`,
        },
        body: encodeDataFrame(reqWire),
      });
      expect(res.status).toBe(200);

      const buf = new Uint8Array(await res.arrayBuffer());
      const { frames } = parseConnectFrames(buf);

      let text = "";
      for (const f of frames) {
        if (f.flag === 0x00) {
          const fields = parseFields(f.payload);
          const tf = findField(fields, 3);
          if (tf) text += new TextDecoder().decode(tf.data);
        }
      }
      expect(text.length).toBeGreaterThan(0);
      expect(text).toContain("22C");
    });
  });

  // ─── 8. Scenario 5: Output Limit ─────────────────────────────────────────

  describe("Scenario 5: Output Limit", () => {
    beforeAll(async () => {
      await fetch(`${baseUrl}/__control/reset`, { method: "POST" });
      await fetch(`${baseUrl}/__control/scenario`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ scenario: "output-limit" }),
      });
    });

    it("returns partial text with STOP_REASON_MAX_TOKENS (3)", async () => {
      const token = "dummy-token";
      const reqWire = buildGetChatMessageRequestWire({
        token,
        modelUid: "glm-5-2",
        prompt: "Long answer",
      });
      const res = await fetch(`${baseUrl}/exa.api_server_pb.ApiServerService/GetChatMessage`, {
        method: "POST",
        headers: {
          "Content-Type": "application/connect+proto",
          Authorization: `Basic ${token}-${token}`,
        },
        body: encodeDataFrame(reqWire),
      });
      expect(res.status).toBe(200);

      const buf = new Uint8Array(await res.arrayBuffer());
      const { frames } = parseConnectFrames(buf);

      let stopReason = null;
      for (const f of frames) {
        if (f.flag === 0x00) {
          const fields = parseFields(f.payload);
          const sr = findField(fields, 5);
          if (sr) stopReason = Number(sr.value);
        }
      }
      expect(stopReason).toBe(3); // STOP_REASON_MAX_TOKENS
    });
  });

  // ─── 9. Scenario 6: Terminal Code-Only Error ─────────────────────────────

  describe("Scenario 6: Terminal Code-Only Error", () => {
    beforeAll(async () => {
      await fetch(`${baseUrl}/__control/reset`, { method: "POST" });
      await fetch(`${baseUrl}/__control/scenario`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ scenario: "code-only-error" }),
      });
    });

    it("returns HTTP 200 with flag 0x02 EndStreamResponse containing code-only error", async () => {
      const token = "dummy-token";
      const reqWire = buildGetChatMessageRequestWire({
        token,
        modelUid: "glm-5-2",
        prompt: "Trigger resource exhausted",
      });
      const res = await fetch(`${baseUrl}/exa.api_server_pb.ApiServerService/GetChatMessage`, {
        method: "POST",
        headers: {
          "Content-Type": "application/connect+proto",
          Authorization: `Basic ${token}-${token}`,
        },
        body: encodeDataFrame(reqWire),
      });
      expect(res.status).toBe(200); // HTTP status remains 200 in Connect

      const buf = new Uint8Array(await res.arrayBuffer());
      const { frames } = parseConnectFrames(buf);
      expect(frames.length).toBe(1);
      expect(frames[0].flag).toBe(0x02);

      const endJson = JSON.parse(new TextDecoder().decode(frames[0].payload));
      expect(endJson.error).toBeDefined();
      expect(endJson.error.code).toBe("resource_exhausted");
      expect(endJson.error.message).toBeUndefined(); // code-only!
    });
  });

  // ─── 10. Scenario 7: Late Terminal Error ─────────────────────────────────

  describe("Scenario 7: Late Terminal Error", () => {
    beforeAll(async () => {
      await fetch(`${baseUrl}/__control/reset`, { method: "POST" });
      await fetch(`${baseUrl}/__control/scenario`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ scenario: "late-error" }),
      });
    });

    it("streams initial data frame, then terminates with EndStreamResponse error", async () => {
      const token = "dummy-token";
      const reqWire = buildGetChatMessageRequestWire({
        token,
        modelUid: "glm-5-2",
        prompt: "Will fail late",
      });
      const res = await fetch(`${baseUrl}/exa.api_server_pb.ApiServerService/GetChatMessage`, {
        method: "POST",
        headers: {
          "Content-Type": "application/connect+proto",
          Authorization: `Basic ${token}-${token}`,
        },
        body: encodeDataFrame(reqWire),
      });
      expect(res.status).toBe(200);

      const buf = new Uint8Array(await res.arrayBuffer());
      const { frames } = parseConnectFrames(buf);
      expect(frames.length).toBe(2);
      expect(frames[0].flag).toBe(0x00); // initial data frame
      expect(frames[1].flag).toBe(0x02); // late error

      const endJson = JSON.parse(new TextDecoder().decode(frames[1].payload));
      expect(endJson.error.code).toBe("unavailable");
      expect(endJson.error.message).toBeDefined();
    });
  });

  // ─── 11. Scenario 8: Transport Cancellation & Deliberate Chunks ─────────

  describe("Scenario 8: Transport Cancellation & Deliberate Chunks", () => {
    beforeAll(async () => {
      await fetch(`${baseUrl}/__control/reset`, { method: "POST" });
      await fetch(`${baseUrl}/__control/scenario`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ scenario: "transport-cancellation" }),
      });
    });

    it("splits chunks deliberately and records connection-close signal when client cancels post-first-byte", async () => {
      const token = "dummy-token";
      const reqWire = buildGetChatMessageRequestWire({
        token,
        modelUid: "glm-5-2",
        prompt: "Cancel me after first byte",
      });

      const abortController = new AbortController();
      const res = await fetch(`${baseUrl}/exa.api_server_pb.ApiServerService/GetChatMessage`, {
        method: "POST",
        headers: {
          "Content-Type": "application/connect+proto",
          Authorization: `Basic ${token}-${token}`,
        },
        body: encodeDataFrame(reqWire),
        signal: abortController.signal,
      });
      expect(res.status).toBe(200);

      const reader = res.body.getReader();
      // Read first chunk (deliberately sent before gate)
      const { value: chunk1, done } = await reader.read();
      expect(done).toBe(false);
      expect(chunk1.length).toBeGreaterThan(0);

      // Subscribe to cancellation event BEFORE triggering abort
      const eventPromise = fetch(`${baseUrl}/__control/wait-event`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ type: "client_closed", timeout_ms: 5000 }),
      });

      // Now cancel the reader/connection post-first-byte
      abortController.abort();
      await reader.cancel().catch(() => {});

      // Wait for server to record client_closed event using bounded event waiter (no fixed sleeps)
      const eventRes = await eventPromise;
      expect(eventRes.status).toBe(200);
      const eventData = await eventRes.json();
      expect(eventData.ok).toBe(true);
      expect(eventData.event.type).toBe("client_closed");

      // Verify sanitized captures reflect early client closure
      const stateRes = await fetch(`${baseUrl}/__control/state`);
      const state = await stateRes.json();
      expect(state.connection_close_count).toBeGreaterThanOrEqual(1);
      const latestCapture = state.captures[state.captures.length - 1];
      expect(latestCapture.client_closed_early).toBe(true);
    });
  });

  // ─── 12. Control API, State and Token Redaction ─────────────────────────

  describe("Control API & Token Redaction", () => {
    it("redacts secret tokens from headers and parsed protobuf bodies in captures", async () => {
      await fetch(`${baseUrl}/__control/reset`, { method: "POST" });

      const secretToken = "super-secret-devin-token-xyz-12345";
      const reqWire = buildGetCascadeModelConfigsRequestWire(secretToken);
      await fetch(`${baseUrl}/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs`, {
        method: "POST",
        headers: {
          "Content-Type": "application/proto",
          Authorization: `Basic ${secretToken}-${secretToken}`,
        },
        body: reqWire,
      });

      const stateRes = await fetch(`${baseUrl}/__control/state`);
      const state = await stateRes.json();
      expect(state.captures.length).toBe(1);

      const capture = state.captures[0];
      const captureStr = JSON.stringify(capture);

      // Raw token MUST NOT appear anywhere in the sanitized state
      expect(captureStr).not.toContain(secretToken);

      // Headers must be sanitized
      expect(capture.headers.authorization).toBe("Basic [REDACTED]-[REDACTED]");
      // Protobuf body metadata must be sanitized
      expect(capture.sanitized_body.api_key).toBe("[REDACTED]");
    });

    it("does not leak configured scenario tokens in POST /__control/scenario or GET /__control/state", async () => {
      await fetch(`${baseUrl}/__control/reset`, { method: "POST" });

      const privateTokenA = "fixture-private-A-998877";
      const privateTokenB = "fixture-private-B-887766";

      // 1. Configure scenario with private tokens
      const setRes = await fetch(`${baseUrl}/__control/scenario`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          scenario: "disjoint-catalogs",
          options: {
            account_a_token: privateTokenA,
            account_b_token: privateTokenB,
          },
        }),
      });
      expect(setRes.status).toBe(200);

      // Regression check: POST response must not echo raw options or contain the tokens
      const postText = await setRes.text();
      expect(postText.includes(privateTokenA)).toBe(false);
      expect(postText.includes(privateTokenB)).toBe(false);
      const postJson = JSON.parse(postText);
      expect(postJson.options).toBeUndefined();

      // Regression check: GET /__control/state must not contain raw tokens in full serialized state
      const stateRes = await fetch(`${baseUrl}/__control/state`);
      expect(stateRes.status).toBe(200);
      const stateText = await stateRes.text();
      expect(stateText.includes(privateTokenA)).toBe(false);
      expect(stateText.includes(privateTokenB)).toBe(false);
      const stateJson = JSON.parse(stateText);
      expect(stateJson.scenario_options.account_a_token).toBe("[REDACTED]");
      expect(stateJson.scenario_options.account_b_token).toBe("[REDACTED]");

      // Verify routing still works despite public state redaction
      const resA = await fetch(`${baseUrl}/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs`, {
        method: "POST",
        headers: {
          "Content-Type": "application/proto",
          Authorization: `Basic ${privateTokenA}-${privateTokenA}`,
        },
        body: buildGetCascadeModelConfigsRequestWire(privateTokenA),
      });
      expect(resA.status).toBe(200);
      const configsA = parseFields(new Uint8Array(await resA.arrayBuffer())).filter((f) => f.fieldNumber === 1);
      expect(configsA.length).toBe(1);
      const uidA = findField(parseFields(configsA[0].data), 22);
      expect(new TextDecoder().decode(uidA.data)).toBe("glm-5-2");

      const resB = await fetch(`${baseUrl}/exa.api_server_pb.ApiServerService/GetCascadeModelConfigs`, {
        method: "POST",
        headers: {
          "Content-Type": "application/proto",
          Authorization: `Basic ${privateTokenB}-${privateTokenB}`,
        },
        body: buildGetCascadeModelConfigsRequestWire(privateTokenB),
      });
      expect(resB.status).toBe(200);
      const configsB = parseFields(new Uint8Array(await resB.arrayBuffer())).filter((f) => f.fieldNumber === 1);
      expect(configsB.length).toBe(1);
      const uidB = findField(parseFields(configsB[0].data), 22);
      expect(new TextDecoder().decode(uidB.data)).toBe("swe-1-7");
    });

    it("resets state, counts, and scenario on POST /__control/reset", async () => {
      const resetRes = await fetch(`${baseUrl}/__control/reset`, { method: "POST" });
      expect(resetRes.status).toBe(200);

      const stateRes = await fetch(`${baseUrl}/__control/state`);
      const state = await stateRes.json();
      expect(state.request_count).toBe(0);
      expect(state.captures.length).toBe(0);
      expect(state.scenario).toBe("default");
    });
  });
});
