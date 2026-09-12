import { describe, it, expect, beforeAll, afterAll } from "bun:test";
import { existsSync, readFileSync, rmSync } from "node:fs";
import * as path from "node:path";
import {
  getFreePort,
  isPortAvailable,
  verifyTcpConnect,
  generateLeadQACommands,
  parseCliArgs,
} from "./runner";

describe("Devin E2E Runner Defects & Invariants", () => {
  describe("Defect 1: No sleep or polling in runner code", () => {
    it("stopExternalStack must not use sleep polling in a while loop", () => {
      const runnerSource = readFileSync(path.join(import.meta.dir, "runner.ts"), "utf-8");
      const stopExternalFn = runnerSource.match(/export async function stopExternalStack[\s\S]*?return\s*\{/);
      expect(stopExternalFn).not.toBeNull();
      expect(stopExternalFn![0]).not.toContain("setTimeout(r, 100)");
      expect(stopExternalFn![0]).not.toContain("while (Date.now()");
    });
  });

  describe("Defect 2: Two-account scope commands in lead QA output", () => {
    it("generateLeadQACommands must provide commands for two-account scope (Account A and Account B)", () => {
      const cmds = generateLeadQACommands("http://127.0.0.1:18801", "devin-dummy-mgmt-key", "http://127.0.0.1:18802");
      // Must contain instructions/commands for two accounts with distinct identities
      expect(cmds).toContain("Two-Account Scope");
      expect(cmds).toContain("devin-alpha");
      expect(cmds).toContain("devin-beta");
      expect(cmds).toContain("disjoint-catalogs");
    });
  });

  describe("Defect 3: Transport cancellation commands in lead QA output", () => {
    it("generateLeadQACommands must provide commands for transport cancellation and wire checks", () => {
      const cmds = generateLeadQACommands("http://127.0.0.1:18801", "devin-dummy-mgmt-key", "http://127.0.0.1:18802");
      expect(cmds).toContain("Transport Cancellation");
      expect(cmds).toContain("transport-cancellation");
      expect(cmds).toContain("/__control/state");
    });
  });

  describe("Defect 4: Maho CLI browser QA commands with explicit tab_id", () => {
    it("generateLeadQACommands must include explicit maho CLI commands with --tab <id> and tab listing", () => {
      const cmds = generateLeadQACommands("http://127.0.0.1:18801", "devin-dummy-mgmt-key", "http://127.0.0.1:18802");
      expect(cmds).toContain("maho tab list");
      expect(cmds).toContain("--tab");
      expect(cmds).toContain("screenshot");
    });
  });

  describe("Defect 5: Gateway logging must not double-lock stdout ReadableStream", () => {
    it("runner.ts must not call proc.stdout.getReader() multiple times on the same process", () => {
      const runnerSource = readFileSync(path.join(import.meta.dir, "runner.ts"), "utf-8");
      // Count proc.stdout.getReader() in startGatewayProcess
      const gatewayFnMatch = runnerSource.match(/async function startGatewayProcess[\s\S]*?return\s*\{/);
      expect(gatewayFnMatch).not.toBeNull();
      const fnBody = gatewayFnMatch![0];
      const getReaderMatches = fnBody.match(/proc\.stdout\.getReader\(\)/g) || [];
      expect(getReaderMatches.length).toBeLessThanOrEqual(1);
    });
  });

  describe("Network and Ephemeral Ports", () => {
    it("getFreePort allocates free port and releases it immediately", async () => {
      const port = await getFreePort("127.0.0.1");
      expect(port).toBeGreaterThan(1024);
      const free = await isPortAvailable(port, "127.0.0.1");
      expect(free).toBe(true);
    });

    it("verifyTcpConnect resolves false on closed port without hanging", async () => {
      const port = await getFreePort("127.0.0.1");
      const connected = await verifyTcpConnect(port, "127.0.0.1", 500);
      expect(connected).toBe(false);
    });
  });

  describe("Isolated Artifacts & Config", () => {
    it("createIsolatedArtifacts creates private directory hierarchy and valid credentials.toml", async () => {
      const { createIsolatedArtifacts } = await import("./runner");
      const mockUrl = "http://127.0.0.1:19999";
      const sessionToken = "test-token-xyz";
      const artifacts = await createIsolatedArtifacts(mockUrl, sessionToken);

      try {
        expect(existsSync(artifacts.rootDir)).toBe(true);
        expect(existsSync(artifacts.authDir)).toBe(true);
        expect(existsSync(artifacts.cliDir)).toBe(true);
        expect(existsSync(artifacts.credentialsFile)).toBe(true);

        const content = readFileSync(artifacts.credentialsFile, "utf-8");
        expect(content).toContain(`windsurf_api_key = "${sessionToken}"`);
        expect(content).toContain(`api_server_url = "${mockUrl}"`);
      } finally {
        rmSync(artifacts.rootDir, { recursive: true, force: true });
      }
    });
  });

  describe("Safe Static Console Server", () => {
    it("startConsoleServer serves index.html read-only and stops cleanly", async () => {
      const { startConsoleServer } = await import("./runner");
      const testHtmlPath = path.join(import.meta.dir, "../../ui/index.html");
      const port = await getFreePort("127.0.0.1");

      const consoleInstance = startConsoleServer(testHtmlPath, port, "127.0.0.1");
      try {
        const resp = await fetch(consoleInstance.url);
        expect(resp.status).toBe(200);
        expect(resp.headers.get("content-type")).toContain("text/html");

        // 405 on POST
        const postResp = await fetch(consoleInstance.url, { method: "POST" });
        expect(postResp.status).toBe(405);

        // 404 on missing
        const notFoundResp = await fetch(`${consoleInstance.url}/nonexistent`);
        expect(notFoundResp.status).toBe(404);
      } finally {
        consoleInstance.stop();
        const released = await isPortAvailable(port, "127.0.0.1");
        expect(released).toBe(true);
      }
    });
  });

  describe("Full E2EStack Process Lifecycle", () => {
    it("spawns isolated gateway + mock + console, verifies ephemeral ports, and cleanly stops with zero leaks", async () => {
      const { E2EStack, parseCliArgs } = await import("./runner");
      const opts = parseCliArgs(["bun", "runner.ts", "start"]);
      const stack = new E2EStack(opts);

      try {
        const state = await stack.start();
        expect(state.status).toBe("ready");
        expect(state.gateway.port).toBeGreaterThan(1024);
        expect(state.mock.port).toBeGreaterThan(1024);
        expect(state.console.port).toBeGreaterThan(1024);

        // Verify gateway healthz responds
        const healthResp = await fetch(`${state.gateway.url}/healthz`);
        expect(healthResp.status).toBe(200);

        // Verify console responds
        const consoleResp = await fetch(state.console.url);
        expect(consoleResp.status).toBe(200);

        // Verify mock responds
        const mockHealthResp = await fetch(`${state.mock.url}/__control/state`);
        expect(mockHealthResp.status).toBe(200);
      } finally {
        const receipt = await stack.stop();
        expect(receipt.killedPids.length).toBeGreaterThanOrEqual(2);
        expect(receipt.releasedPorts.length).toBeGreaterThanOrEqual(3);
        expect(receipt.artifactsCleaned).toBe(true);
      }
    }, 20000);
  });
});
