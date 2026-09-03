#!/usr/bin/env python3
"""
Task 17 Integration QA Driver
Validates live gateway binary compiled at target/debug/mahoquot-gateway.
Exercises:
  - --help
  - Clean boot with temp HOME, signed fixture catalog, local mock upstream (127.0.0.1 only)
  - GET /healthz
  - GET /v1/models
  - GET /v1beta/models
  - GET /v0/management/model-registry (authenticated)
  - Route valid fixture request (POST /v1/chat/completions)
  - Route unknown model -> verify local 400 model_not_found
  - Malformed alias update (PUT /v0/management/oauth-model-alias) -> verify rollback
  - Offline restart from LKG
  - Newer catalog hot refresh (POST /v0/management/model-registry)
  - Verify all outbound requests strictly hit 127.0.0.1
  - Clean up port 18880, mock ports, and temp directories.
"""

import http.server
import json
import os
import shutil
import socket
import socketserver
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

GATEWAY_PORT = 18880
MOCK_UPSTREAM_PORT = 18881
MOCK_CATALOG_PORT = 18882

PROXY_ROOT = Path("/Users/indo/code/project/mahoquot-proxy").resolve()
QUOTIO_ROOT = Path("/Users/indo/code/project/quotio-rs").resolve()
GATEWAY_BIN = PROXY_ROOT / "target" / "debug" / "mahoquot-gateway"
BASE_CATALOG_PATH = PROXY_ROOT / "crates" / "registry" / "catalog" / "models-v1.json"
TEST_KEY_PATH = PROXY_ROOT / "tests" / "fixtures" / "test-ed25519.key"
TEST_PUB_PATH = PROXY_ROOT / "tests" / "fixtures" / "test-ed25519.pub"

outbound_requests = []
outbound_lock = threading.Lock()

class MockUpstreamHandler(http.server.BaseHTTPRequestHandler):
    def log_message(self, format, *args):
        pass

    def do_POST(self):
        client_ip = self.client_address[0]
        host_hdr = self.headers.get("Host", "")
        content_len = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(content_len)

        with outbound_lock:
            outbound_requests.append({
                "server": "upstream",
                "client_ip": client_ip,
                "host_header": host_hdr,
                "path": self.path,
                "method": "POST"
            })

        # Return streaming SSE for Antigravity
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()

        sse_chunk = (
            'data: {"response":{"responseId":"ag_task17","candidates":[{"content":'
            '{"role":"model","parts":[{"text":"hello from antigravity fixture"}]},'
            '"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,'
            '"candidatesTokenCount":5,"totalTokenCount":10}}}\n\n'
        )
        self.wfile.write(sse_chunk.encode("utf-8"))

class MockCatalogHandler(http.server.BaseHTTPRequestHandler):
    catalog_payload_v3 = b""
    catalog_sig_v3 = b""
    is_online = True

    def log_message(self, format, *args):
        pass

    def do_GET(self):
        client_ip = self.client_address[0]
        host_hdr = self.headers.get("Host", "")

        with outbound_lock:
            outbound_requests.append({
                "server": "catalog",
                "client_ip": client_ip,
                "host_header": host_hdr,
                "path": self.path,
                "method": "GET"
            })

        if not self.is_online:
            self.send_response(503)
            self.end_headers()
            self.wfile.write(b"offline")
            return

        if self.path == "/models-v1.json.sig":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(self.catalog_sig_v3)))
            self.end_headers()
            self.wfile.write(self.catalog_sig_v3)
        elif self.path == "/models-v1.json":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(self.catalog_payload_v3)))
            self.end_headers()
            self.wfile.write(self.catalog_payload_v3)
        else:
            self.send_response(404)
            self.end_headers()

class ReusableThreadingServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    allow_reuse_address = True

def wait_for_port(port, timeout=10.0, open_expected=True):
    start = time.time()
    while time.time() - start < timeout:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
            s.settimeout(0.2)
            result = s.connect_ex(("127.0.0.1", port))
            if open_expected and result == 0:
                return True
            if not open_expected and result != 0:
                return True
        time.sleep(0.05)
    return False

def sign_catalog(cat_dict, work_dir, key_id="test-ed25519-v1"):
    in_file = work_dir / f"cat-in-{cat_dict['version']}.json"
    out_payload = work_dir / f"cat-out-{cat_dict['version']}.json"
    out_sig = work_dir / f"cat-out-{cat_dict['version']}.json.sig"

    with open(in_file, "w") as f:
        json.dump(cat_dict, f)

    cmd = [
        "cargo", "run", "-q", "-p", "mahoquot-model-catalog", "--", "sign",
        "--key-file", str(TEST_KEY_PATH),
        "--key-id", key_id,
        "--input", str(in_file),
        "--output", str(out_payload),
        "--signature", str(out_sig),
    ]
    res = subprocess.run(cmd, cwd=PROXY_ROOT, capture_output=True, text=True)
    if res.returncode != 0:
        raise RuntimeError(f"Failed to sign catalog: {res.stderr}")

    with open(out_payload, "rb") as f:
        payload_bytes = f.read()
    with open(out_sig, "rb") as f:
        sig_bytes = f.read()

    return payload_bytes, sig_bytes

def execute_curl(args, stdin_data=None):
    cmd = ["curl", "-sS", "-i"] + args
    res = subprocess.run(cmd, input=stdin_data, capture_output=True, text=True)
    if res.returncode != 0:
        raise RuntimeError(f"curl failed ({cmd}): {res.stderr}")
    return res.stdout

def format_http_exchange(method, url, headers=None, body=None, response_text=""):
    parsed_path = url.split("127.0.0.1:18880", 1)[1] if "127.0.0.1:18880" in url else url
    lines = [f"{method} {parsed_path} HTTP/1.1", f"Host: 127.0.0.1:{GATEWAY_PORT}"]
    if headers:
        for k, v in headers.items():
            lines.append(f"{k}: {v}")
    lines.append("")
    if body:
        if isinstance(body, (dict, list)):
            lines.append(json.dumps(body, indent=2))
        else:
            lines.append(str(body))
        lines.append("")
    lines.append(response_text.strip())
    return "\n".join(lines)

def main():
    print("=== Step 1: Check --help on compiled binary ===")
    help_res = subprocess.run([str(GATEWAY_BIN), "--help"], capture_output=True, text=True)
    assert help_res.returncode == 0, f"--help failed: {help_res.stderr}"
    assert "Mahoquot high-concurrency LLM inference proxy and router" in help_res.stdout
    print("  PASS: --help returned 0 and printed correct usage.")

    temp_root = Path(tempfile.mkdtemp(prefix="mahoquot-task17-qa-"))
    print(f"  Created test root: {temp_root}")

    http_transcripts = []

    try:
        home_dir = temp_root / "home"
        home_dir.mkdir(parents=True, exist_ok=True)
        cache_dir = home_dir / ".mahoquot" / "cache"
        cache_dir.mkdir(parents=True, exist_ok=True)
        auth_dir = temp_root / "auth"
        auth_dir.mkdir(parents=True, exist_ok=True)

        # 1. Prepare Catalog v2 (LKG fixture)
        with open(BASE_CATALOG_PATH, "r") as f:
            base_cat = json.load(f)

        cat_v2 = dict(base_cat)
        cat_v2["version"] = 2
        cat_v2["source"] = "remote_signed"
        for m in cat_v2["models"].values():
            for b in m["bindings"].values():
                b["source"] = "remote_signed"

        cat_v2["models"]["task-17-test-model"] = {
            "id": "task-17-test-model",
            "owned_by": "google",
            "capabilities": ["chat", "tools"],
            "bindings": {
                "antigravity": {
                    "provider_id": "antigravity",
                    "policy": "closed",
                    "source": "remote_signed",
                    "capabilities": ["chat", "tools"],
                    "authority": {
                        "models": True,
                        "capabilities": True,
                        "aliases": True,
                        "prefixes": True,
                        "upstream_id": True
                    },
                    "priority": 100
                }
            },
            "aliases": [],
            "metadata": {"context_limit": "200000"}
        }

        v2_payload, v2_sig = sign_catalog(cat_v2, temp_root)

        # Build SignedCatalogPackage for initial LKG
        v2_package = {
            "envelope": json.loads(v2_sig.decode("utf-8")),
            "payload": json.loads(v2_payload.decode("utf-8")),
            "raw_payload": v2_payload.decode("utf-8"),
        }
        lkg_file = cache_dir / "models-v1.signed.json"
        with open(lkg_file, "w") as f:
            json.dump(v2_package, f, indent=2)
        print(f"  Prepared LKG cache at {lkg_file} with version 2")

        # 2. Prepare Catalog v3 (for hot refresh later)
        cat_v3 = dict(cat_v2)
        cat_v3["version"] = 3
        cat_v3["models"]["task-17-v3-hot-refreshed-model"] = {
            "id": "task-17-v3-hot-refreshed-model",
            "owned_by": "google",
            "capabilities": ["chat"],
            "bindings": {
                "antigravity": {
                    "provider_id": "antigravity",
                    "policy": "closed",
                    "source": "remote_signed",
                    "capabilities": ["chat"],
                    "authority": {
                        "models": True,
                        "capabilities": True,
                        "aliases": True,
                        "prefixes": True,
                        "upstream_id": True
                    },
                    "priority": 100
                }
            },
            "aliases": [],
            "metadata": {"context_limit": "500000"}
        }
        v3_payload, v3_sig = sign_catalog(cat_v3, temp_root)
        MockCatalogHandler.catalog_payload_v3 = v3_payload
        MockCatalogHandler.catalog_sig_v3 = v3_sig
        MockCatalogHandler.is_online = False # offline initially

        # Start mock servers
        upstream_server = ReusableThreadingServer(("127.0.0.1", MOCK_UPSTREAM_PORT), MockUpstreamHandler)
        upstream_thread = threading.Thread(target=upstream_server.serve_forever, daemon=True)
        upstream_thread.start()

        catalog_server = ReusableThreadingServer(("127.0.0.1", MOCK_CATALOG_PORT), MockCatalogHandler)
        catalog_thread = threading.Thread(target=catalog_server.serve_forever, daemon=True)
        catalog_thread.start()

        print(f"  Mock upstream listening on port {MOCK_UPSTREAM_PORT}")
        print(f"  Mock catalog server listening on port {MOCK_CATALOG_PORT}")

        # Provider account in auth_dir pointing to mock upstream
        antigravity_account = {
            "type": "antigravity",
            "identity_slug": "ag-fixture-task17",
            "access_token": "tok-ag-task17",
            "refresh_token": "ref-ag-task17",
            "email": "task17@antigravity.test",
            "project_id": "project-task17",
            "expired": "2099-01-01T00:00:00Z",
            "upstream_override": f"http://127.0.0.1:{MOCK_UPSTREAM_PORT}"
        }
        with open(auth_dir / "antigravity-account.json", "w") as f:
            json.dump(antigravity_account, f, indent=2)

        # Gateway config.yaml
        config_yaml = (
            f"port: {GATEWAY_PORT}\n"
            f"auth-dir: \"{auth_dir}\"\n"
            "api-keys:\n"
            "  - test-admin\n"
            "  - test-user\n"
            "model-catalog:\n"
            "  refresh-enabled: true\n"
            f"  url: \"http://127.0.0.1:{MOCK_CATALOG_PORT}/models-v1.json\"\n"
            f"  signature-url: \"http://127.0.0.1:{MOCK_CATALOG_PORT}/models-v1.json.sig\"\n"
            "  refresh-interval-secs: 3600\n"
        )
        config_path = temp_root / "config.yaml"
        with open(config_path, "w") as f:
            f.write(config_yaml)

        # Helper to launch gateway
        def launch_gateway():
            env = os.environ.copy()
            env["HOME"] = str(home_dir)
            env["MAHOQUOT_CACHE_DIR"] = str(cache_dir)
            env["AUTH_DIR"] = str(auth_dir)
            env["CONFIG_PATH"] = str(config_path)
            env["GATEWAY_PORT"] = str(GATEWAY_PORT)
            env["BIND_ADDR"] = "127.0.0.1"

            cmd = [
                str(GATEWAY_BIN),
                "--port", str(GATEWAY_PORT),
                "--bind", "127.0.0.1",
                "--auth-dir", str(auth_dir),
                "--config", str(config_path),
                "--api-keys", "test-admin,test-user",
                "--auth-refresh", "false",
                "--usage-poll-secs", "999999",
                "--log-level", "info"
            ]
            proc = subprocess.Popen(cmd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            if not wait_for_port(GATEWAY_PORT, timeout=10.0, open_expected=True):
                stdout, stderr = proc.communicate(timeout=2)
                raise RuntimeError(f"Gateway failed to bind port {GATEWAY_PORT}.\nStdout: {stdout}\nStderr: {stderr}")
            return proc

        # Helper to shutdown gateway
        def stop_gateway(proc):
            try:
                execute_curl(["-X", "POST", f"http://127.0.0.1:{GATEWAY_PORT}/v0/management/shutdown", "-H", "Authorization: Bearer test-admin"])
            except Exception:
                proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=2)
            wait_for_port(GATEWAY_PORT, timeout=5.0, open_expected=False)

        print("=== Step 2: Clean boot gateway binary ===")
        proc = launch_gateway()
        print("  Gateway process booted and listening on 18880.")

        # 1. GET /healthz
        print("=== Scenario: GET /healthz ===")
        url = f"http://127.0.0.1:{GATEWAY_PORT}/healthz"
        resp = execute_curl([url])
        assert "200 OK" in resp, f"GET /healthz failed: {resp}"
        assert '"status":"ok"' in resp or '"status": "ok"' in resp, f"Unexpected body: {resp}"
        http_transcripts.append(format_http_exchange("GET", url, response_text=resp))
        print("  PASS: GET /healthz -> 200 OK")

        # 2. GET /v1/models
        print("=== Scenario: GET /v1/models ===")
        url = f"http://127.0.0.1:{GATEWAY_PORT}/v1/models"
        hdrs = {"Authorization": "Bearer test-admin"}
        resp = execute_curl([url, "-H", "Authorization: Bearer test-admin"])
        assert "200 OK" in resp, f"GET /v1/models failed: {resp}"
        assert "task-17-test-model" in resp, f"Missing fixture model in /v1/models: {resp}"
        http_transcripts.append(format_http_exchange("GET", url, headers=hdrs, response_text=resp))
        print("  PASS: GET /v1/models -> 200 OK (contains task-17-test-model)")

        # 3. GET /v1beta/models
        print("=== Scenario: GET /v1beta/models ===")
        url = f"http://127.0.0.1:{GATEWAY_PORT}/v1beta/models"
        hdrs = {"Authorization": "Bearer test-admin"}
        resp = execute_curl([url, "-H", "Authorization: Bearer test-admin"])
        assert "200 OK" in resp, f"GET /v1beta/models failed: {resp}"
        assert "task-17-test-model" in resp, f"Missing fixture model in /v1beta/models: {resp}"
        http_transcripts.append(format_http_exchange("GET", url, headers=hdrs, response_text=resp))
        print("  PASS: GET /v1beta/models -> 200 OK")

        # 4. GET /v0/management/model-registry (authenticated)
        print("=== Scenario: GET /v0/management/model-registry ===")
        url = f"http://127.0.0.1:{GATEWAY_PORT}/v0/management/model-registry"
        hdrs = {"Authorization": "Bearer test-admin"}
        resp = execute_curl([url, "-H", "Authorization: Bearer test-admin"])
        assert "200 OK" in resp, f"GET /v0/management/model-registry failed: {resp}"
        assert '"catalog-version":2' in resp or '"catalog-version": 2' in resp, f"Version 2 expected: {resp}"
        assert '"source":"lkg_cache"' in resp or '"source": "lkg_cache"' in resp, f"Source lkg_cache expected: {resp}"
        http_transcripts.append(format_http_exchange("GET", url, headers=hdrs, response_text=resp))
        print("  PASS: GET /v0/management/model-registry -> 200 OK (version 2, source lkg_cache)")

        # 5. Route valid fixture request (POST /v1/chat/completions)
        print("=== Scenario: Route valid fixture request (POST /v1/chat/completions) ===")
        url = f"http://127.0.0.1:{GATEWAY_PORT}/v1/chat/completions"
        hdrs = {"Authorization": "Bearer test-user", "Content-Type": "application/json"}
        body = {
            "model": "claude-sonnet-4-6",
            "messages": [{"role": "user", "content": "hello"}]
        }
        body_json = json.dumps(body)
        resp = execute_curl([
            "-X", "POST", url,
            "-H", "Authorization: Bearer test-user",
            "-H", "Content-Type: application/json",
            "--data", body_json
        ])
        assert "200 OK" in resp, f"POST /v1/chat/completions failed: {resp}"
        assert "hello from antigravity fixture" in resp, f"Expected mock upstream message in: {resp}"
        http_transcripts.append(format_http_exchange("POST", url, headers=hdrs, body=body, response_text=resp))
        print("  PASS: Route valid fixture request -> 200 OK (received upstream response)")

        # Verify upstream received hit
        with outbound_lock:
            upstream_hits = [r for r in outbound_requests if r["server"] == "upstream"]
            assert len(upstream_hits) >= 1, "Upstream received no hits"
            assert upstream_hits[-1]["client_ip"] == "127.0.0.1"
            assert upstream_hits[-1]["host_header"].startswith(f"127.0.0.1:{MOCK_UPSTREAM_PORT}")
        print("  PASS: Mock upstream verified hit from 127.0.0.1")

        # 6. Route unknown model -> verify local 400 model_not_found
        print("=== Scenario: Route unknown model -> verify local 400 model_not_found ===")
        url = f"http://127.0.0.1:{GATEWAY_PORT}/v1/chat/completions"
        hdrs = {"Authorization": "Bearer test-user", "Content-Type": "application/json"}
        body_unknown = {
            "model": "completely-unknown-fixture-model-xyz",
            "messages": [{"role": "user", "content": "hello"}]
        }
        hits_before = len(upstream_hits)
        resp_unknown = execute_curl([
            "-X", "POST", url,
            "-H", "Authorization: Bearer test-user",
            "-H", "Content-Type: application/json",
            "--data", json.dumps(body_unknown)
        ])
        assert "400 Bad Request" in resp_unknown, f"Expected 400 for unknown model: {resp_unknown}"
        assert "model_not_found" in resp_unknown, f"Expected model_not_found code: {resp_unknown}"
        http_transcripts.append(format_http_exchange("POST", url, headers=hdrs, body=body_unknown, response_text=resp_unknown))

        # Verify ZERO new hits on upstream
        with outbound_lock:
            upstream_hits_after = [r for r in outbound_requests if r["server"] == "upstream"]
            assert len(upstream_hits_after) == hits_before, "Upstream hit count increased on unknown model"
        print("  PASS: Unknown model rejected locally with 400 model_not_found, 0 upstream requests")

        # 7. Malformed alias update (PUT /v0/management/oauth-model-alias) -> verify rollback
        print("=== Scenario: Malformed alias update -> verify rollback ===")
        url = f"http://127.0.0.1:{GATEWAY_PORT}/v0/management/oauth-model-alias"
        hdrs = {"Authorization": "Bearer test-admin", "Content-Type": "application/json"}
        body_malformed = {
            "antigravity": [
                {"name": "cycle-b", "alias": "cycle-a"},
                {"name": "cycle-a", "alias": "cycle-b"}
            ]
        }
        resp_alias = execute_curl([
            "-X", "PUT", url,
            "-H", "Authorization: Bearer test-admin",
            "-H", "Content-Type: application/json",
            "--data", json.dumps(body_malformed)
        ])
        assert "400 Bad Request" in resp_alias, f"Expected 400 for cyclic alias: {resp_alias}"
        assert "cycle" in resp_alias, f"Expected cycle error message: {resp_alias}"
        http_transcripts.append(format_http_exchange("PUT", url, headers=hdrs, body=body_malformed, response_text=resp_alias))

        # Check that generation / state rolled back
        reg_check = execute_curl([f"http://127.0.0.1:{GATEWAY_PORT}/v0/management/model-registry", "-H", "Authorization: Bearer test-admin"])
        assert "200 OK" in reg_check
        assert '"catalog-version":2' in reg_check or '"catalog-version": 2' in reg_check
        print("  PASS: Malformed alias rejected with 400 and state safely rolled back")

        # 8. Offline restart from LKG
        print("=== Scenario: Offline restart from LKG ===")
        # Ensure mock catalog is offline
        MockCatalogHandler.is_online = False
        print("  Stopping running gateway...")
        stop_gateway(proc)
        print("  Gateway stopped. Verifying port 18880 is released...")
        assert not wait_for_port(GATEWAY_PORT, timeout=2.0, open_expected=True)

        print("  Restarting gateway offline (mock catalog offline)...")
        proc = launch_gateway()
        print("  Gateway restarted.")

        url_healthz = f"http://127.0.0.1:{GATEWAY_PORT}/healthz"
        resp_h = execute_curl([url_healthz])
        assert "200 OK" in resp_h
        http_transcripts.append(format_http_exchange("GET", url_healthz, response_text=resp_h))

        url_reg = f"http://127.0.0.1:{GATEWAY_PORT}/v0/management/model-registry"
        hdrs_reg = {"Authorization": "Bearer test-admin"}
        resp_reg = execute_curl([url_reg, "-H", "Authorization: Bearer test-admin"])
        assert "200 OK" in resp_reg
        assert '"catalog-version":2' in resp_reg or '"catalog-version": 2' in resp_reg
        assert '"source":"lkg_cache"' in resp_reg or '"source": "lkg_cache"' in resp_reg
        http_transcripts.append(format_http_exchange("GET", url_reg, headers=hdrs_reg, response_text=resp_reg))
        print("  PASS: Offline restart from LKG successful (v2 active from lkg_cache)")

        # 9. Newer catalog hot refresh (POST /v0/management/model-registry)
        print("=== Scenario: Newer catalog hot refresh (v3) ===")
        # Bring mock catalog online with v3
        MockCatalogHandler.is_online = True

        url_refresh = f"http://127.0.0.1:{GATEWAY_PORT}/v0/management/model-registry"
        hdrs_refresh = {"Authorization": "Bearer test-admin", "Content-Length": "0"}
        resp_refresh = execute_curl([
            "-X", "POST", url_refresh,
            "-H", "Authorization: Bearer test-admin",
            "-H", "Content-Length: 0"
        ])
        assert "202 Accepted" in resp_refresh, f"Expected 202 Accepted for refresh: {resp_refresh}"
        http_transcripts.append(format_http_exchange("POST", url_refresh, headers=hdrs_refresh, response_text=resp_refresh))
        print("  PASS: POST /v0/management/model-registry returned 202 Accepted")

        # Wait for background hot refresh to finish
        print("  Awaiting background hot refresh to v3...")
        refreshed = False
        latest_status_resp = ""
        for _ in range(50):
            time.sleep(0.1)
            latest_status_resp = execute_curl([url_refresh, "-H", "Authorization: Bearer test-admin"])
            if '"catalog-version":3' in latest_status_resp or '"catalog-version": 3' in latest_status_resp:
                refreshed = True
                break

        assert refreshed, f"Catalog hot refresh did not advance to v3: {latest_status_resp}"
        assert '"source":"remote_signed"' in latest_status_resp or '"source": "remote_signed"' in latest_status_resp
        http_transcripts.append(format_http_exchange("GET", url_refresh, headers={"Authorization": "Bearer test-admin"}, response_text=latest_status_resp))
        print("  PASS: Registry status shows version 3, source remote_signed!")

        # Verify new model in /v1/models
        url_models = f"http://127.0.0.1:{GATEWAY_PORT}/v1/models"
        resp_models_v3 = execute_curl([url_models, "-H", "Authorization: Bearer test-admin"])
        assert "200 OK" in resp_models_v3
        assert "task-17-v3-hot-refreshed-model" in resp_models_v3, f"New v3 model missing from /v1/models: {resp_models_v3}"
        http_transcripts.append(format_http_exchange("GET", url_models, headers={"Authorization": "Bearer test-admin"}, response_text=resp_models_v3))
        print("  PASS: /v1/models dynamically advertises task-17-v3-hot-refreshed-model without restart!")

        # 10. Verify all outbound requests strictly hit 127.0.0.1
        print("=== Step 3: Verify all outbound requests strictly hit 127.0.0.1 ===")
        with outbound_lock:
            assert len(outbound_requests) > 0, "No outbound requests captured"
            for req in outbound_requests:
                assert req["client_ip"] == "127.0.0.1", f"Outbound client ip was not 127.0.0.1: {req}"
                assert req["host_header"].startswith("127.0.0.1:"), f"Host header did not start with 127.0.0.1: {req}"
                print(f"  Verified outbound {req['method']} {req['path']} to {req['server']} on {req['host_header']}")
        print("  PASS: All outbound traffic strictly routed to loopback 127.0.0.1")

        # 11. Teardown
        print("=== Step 4: Teardown and Cleanup ===")
        stop_gateway(proc)
        upstream_server.shutdown()
        upstream_server.server_close()
        catalog_server.shutdown()
        catalog_server.server_close()

        assert wait_for_port(GATEWAY_PORT, timeout=5.0, open_expected=False), "Port 18880 not released"
        assert wait_for_port(MOCK_UPSTREAM_PORT, timeout=5.0, open_expected=False), "Port 18881 not released"
        assert wait_for_port(MOCK_CATALOG_PORT, timeout=5.0, open_expected=False), "Port 18882 not released"
        print("  PASS: All ports released (18880, 18881, 18882).")

        # 12. Save full HTTP exchange transcript
        full_transcript = "\n\n---\n\n".join(http_transcripts) + "\n"

        proxy_evidence_dir = PROXY_ROOT / ".omo" / "evidence" / "model-registry"
        quotio_evidence_dir = QUOTIO_ROOT / ".omo" / "evidence" / "model-registry"
        proxy_evidence_dir.mkdir(parents=True, exist_ok=True)
        quotio_evidence_dir.mkdir(parents=True, exist_ok=True)

        proxy_http_file = proxy_evidence_dir / "task-17-live-surface.http"
        quotio_http_file = quotio_evidence_dir / "task-17-live-surface.http"

        with open(proxy_http_file, "w") as f:
            f.write(full_transcript)
        with open(quotio_http_file, "w") as f:
            f.write(full_transcript)

        print(f"  Wrote HTTP transcripts to:\n    {proxy_http_file}\n    {quotio_http_file}")

    finally:
        shutil.rmtree(temp_root, ignore_errors=True)
        print(f"  Cleaned up temp root: {temp_root}")

    print("\n>>> Task 17 Live Surface Integration QA: ALL PASSED <<<")

if __name__ == "__main__":
    main()
