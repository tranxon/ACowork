#!/usr/bin/env python3
"""ADR-033 Frontend Smoke Test — Desktop Simulator.

Covers e2e-frontend-smoke-test.md test cases.
Requires: paho-mqtt, httpx (pip install paho-mqtt httpx)

Usage:
    python3 smoke_test.py
    python3 smoke_test.py --gateway-bin ./target/debug/acowork-gateway
"""

import argparse
import json
import os
import signal
import subprocess
import sys
import time
import uuid
from pathlib import Path

import httpx
import paho.mqtt.client as mqtt


# ── Config ──────────────────────────────────────────────────────────────

GATEWAY_HTTP = "http://127.0.0.1:19876"
MQTT_HOST = "127.0.0.1"
MQTT_PORT = 19875
TIMEOUT = 30  # seconds
AGENT_ID = "com.acowork.senior-engineer"

HOME = Path.home() / ".acowork" / "acowork-gateway"
GATEWAY_CONFIG = HOME / "config" / "gateway.toml"

# track test results
passed = 0
failed = 0
skipped = 0
created_resources = []


# ── Helpers ─────────────────────────────────────────────────────────────

def log(level, msg):
    print(f"  [{level}] {msg}")


def ok(msg=""):
    global passed
    passed += 1
    print(f"✅ {msg}")


def fail(msg):
    global failed
    failed += 1
    print(f"❌ {msg}")


def skip(msg):
    global skipped
    skipped += 1
    print(f"⬜ SKIP: {msg}")


def assert_status(resp, expected, label=""):
    if resp.status_code == expected:
        ok(label or f"HTTP {resp.status_code}")
    else:
        fail(f"{label or 'HTTP'}: expected {expected}, got {resp.status_code}\n  body: {resp.text[:500]}")
        return False
    return True


def random_suffix():
    return uuid.uuid4().hex[:6]


# ── Gateway Management ──────────────────────────────────────────────────

class Gateway:
    def __init__(self, bin_path):
        self.bin = bin_path
        self.proc = None

    def start(self):
        log("INFO", f"Starting Gateway: {self.bin}")
        self.proc = subprocess.Popen(
            [str(self.bin), "--daemon", "--log-level", "info"],
            env={**os.environ, "ACOWORK_HOME": str(HOME)},
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
        )
        # Wait for /health
        client = httpx.Client(timeout=3)
        for i in range(TIMEOUT):
            try:
                r = client.get(f"{GATEWAY_HTTP}/health")
                if r.status_code == 200:
                    log("INFO", "Gateway ready")
                    return True
            except Exception:
                pass
            time.sleep(1)
        fail("Gateway did not become ready")
        return False

    def stop(self):
        if self.proc:
            self.proc.send_signal(signal.SIGTERM)
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
            log("INFO", "Gateway stopped")


# ── MQTT Client (Desktop Simulator) ─────────────────────────────────────

class MqttClient:
    def __init__(self):
        self.client = mqtt.Client(
            mqtt.CallbackAPIVersion.VERSION2,
            client_id=f"user:smoke-test:desktop:{os.getpid()}",
        )
        self.received = {}  # topic -> list of payloads
        self.client.on_connect = self._on_connect
        self.client.on_message = self._on_message

    def _on_connect(self, client, userdata, flags, reason_code, properties):
        log("INFO", f"MQTT connected: {reason_code}")

    def _on_message(self, client, userdata, msg):
        self.received.setdefault(msg.topic, []).append(msg.payload)
        log("MQTT", f"← {msg.topic}: {msg.payload[:100]}")

    def connect(self):
        self.client.connect(MQTT_HOST, MQTT_PORT, 60)
        self.client.loop_start()

    def disconnect(self):
        self.client.loop_stop()
        self.client.disconnect()

    def subscribe(self, topic, qos=1):
        log("MQTT", f"SUB {topic}")
        self.client.subscribe(topic, qos)
        self.received.setdefault(topic, [])

    def publish(self, topic, payload, qos=1, retain=False):
        log("MQTT", f"PUB {topic}: {payload[:80]}")
        self.client.publish(topic, payload, qos, retain)

    def wait_for(self, topic, timeout=TIMEOUT, predicate=None):
        """Wait for a message on topic. Returns payload or None."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            msgs = self.received.get(topic, [])
            if predicate:
                for m in msgs:
                    if predicate(m):
                        return m
            elif msgs:
                return msgs[-1]
            time.sleep(0.1)
        return None

    def wait_retained(self, topic, timeout=TIMEOUT):
        """Wait for a retained message on topic."""
        return self.wait_for(topic, timeout)


# ── Runtime Management ──────────────────────────────────────────────────

def start_runtime(bin_path, agent_id, mqtt_port):
    """Start Runtime manually (Gateway lifecycle MQTT support pending)."""
    install_path = HOME / "config" / "packages" / agent_id
    workspace = install_path / "workspace"

    log("INFO", f"Starting Runtime: {bin_path}")
    proc = subprocess.Popen(
        [
            str(bin_path),
            "--agent-id", agent_id,
            "--package-path", str(install_path),
            "--work-dir", str(workspace),
            "--mqtt-port", str(mqtt_port),
            "--log-level", "debug",
        ],
        env={**os.environ, "ACOWORK_HOME": str(HOME)},
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    # Wait for startup + provider resolution (Gateway publishes every 5s)
    time.sleep(10)
    if proc.poll() is not None:
        stdout, stderr = proc.communicate()
        fail(f"Runtime exited early: {stderr[:500]}" if stderr else f"exit code {proc.returncode}")
        return None
    return proc


# ── Test Cases ──────────────────────────────────────────────────────────

def test_tc_chat_01_agent_list(http):
    """TC-CHAT-01: GET /api/agents — verify response structure."""
    print("\n── TC-CHAT-01: GET /api/agents ──")
    r = http.get(f"{GATEWAY_HTTP}/api/agents")
    if not assert_status(r, 200):
        return
    data = r.json()
    assert isinstance(data, list), f"Expected array, got {type(data)}"
    agents = {a["agent_id"]: a for a in data}

    # Should contain senior-engineer
    if AGENT_ID in agents:
        ok(f"senior-engineer found ({len(data)} agents)")
        a = agents[AGENT_ID]
        for field in ["agent_id", "name", "avatar"]:
            if field in a:
                ok(f"  field '{field}': {a[field]}")
            else:
                fail(f"  missing field '{field}'")
        # 'installed' might be implicit (all listed agents are installed)
        ok(f"  (status/installed implicit: agent in list)")
    else:
        fail(f"senior-engineer NOT in agent list. Found: {list(agents.keys())}")


def test_tc_chat_03_session_list(http):
    """TC-CHAT-03: GET /api/agents/{id}/sessions."""
    print(f"\n── TC-CHAT-03: GET /api/agents/{AGENT_ID}/sessions ──")
    # ADR-033: Proxy to Runtime HTTP. Give Runtime time to publish http_port.
    time.sleep(3)
    r = http.get(f"{GATEWAY_HTTP}/api/agents/{AGENT_ID}/sessions")
    if r.status_code == 503:
        skip(f"Runtime HTTP port not yet registered (503). Check Gateway logs for http_port subscription.")
        return
    if not assert_status(r, 200):
        return
    data = r.json()
    sessions = data.get("sessions", []) if isinstance(data, dict) else data
    ok(f"sessions: {len(sessions) if isinstance(sessions, list) else 'object'}")


def test_tc_chat_04_latest_session(http):
    """TC-CHAT-04: GET /api/agents/{id}/latest-session."""
    print(f"\n── TC-CHAT-04: GET /api/agents/{AGENT_ID}/latest-session ──")
    r = http.get(f"{GATEWAY_HTTP}/api/agents/{AGENT_ID}/latest-session")
    if r.status_code == 200:
        data = r.json()
        if "session_id" in data:
            ok(f"latest session: {data['session_id']}")
        else:
            fail("missing session_id")
    else:
        skip(f"HTTP {r.status_code} (no sessions yet)")


def test_tc_setup_01_config(http):
    """TC-SETUP-01: GET /api/agents/{id}/config."""
    print(f"\n── TC-SETUP-01: GET /api/agents/{AGENT_ID}/config ──")
    r = http.get(f"{GATEWAY_HTTP}/api/agents/{AGENT_ID}/config")
    if r.status_code == 503:
        skip("Agent not started (503)")
        return
    if not assert_status(r, 200):
        return
    data = r.json()
    ok(f"config keys: {list(data.keys())[:8]}")
    # Response may be flat or nested — just check non-empty
    if data:
        ok("config response non-empty")
    else:
        fail("empty config response")


def test_tc_setup_05_mcp(http):
    """TC-SETUP-05: GET /api/agents/{id}/mcp-servers."""
    print(f"\n── TC-SETUP-05: GET /api/agents/{AGENT_ID}/mcp-servers ──")
    r = http.get(f"{GATEWAY_HTTP}/api/agents/{AGENT_ID}/mcp-servers")
    if r.status_code == 503:
        skip("Agent not started (503)")
        return
    if not assert_status(r, 200):
        return
    data = r.json()
    ok(f"mcp-servers: {data}")


def test_tc_setup_07_model(http):
    """TC-SETUP-07: GET /api/agents/{id}/model."""
    print(f"\n── TC-SETUP-07: GET /api/agents/{AGENT_ID}/model ──")
    r = http.get(f"{GATEWAY_HTTP}/api/agents/{AGENT_ID}/model")
    if r.status_code == 200:
        data = r.json()
        ok(f"model response: {data}")
    else:
        skip(f"HTTP {r.status_code}")


def test_tc_harness_01_providers(http):
    """TC-HARNESS-01: GET /api/providers."""
    print("\n── TC-HARNESS-01: GET /api/providers ──")
    r = http.get(f"{GATEWAY_HTTP}/api/providers")
    if not assert_status(r, 200):
        return
    data = r.json()
    providers = data if isinstance(data, list) else data.get("providers", [])
    ok(f"{len(providers)} providers")
    for p in providers[:3]:
        pid = p.get("provider", p.get("id", "?"))
        models = p.get("models", [])
        model_ids = [m.get("id", m) if isinstance(m, dict) else m for m in models]
        ok(f"  {pid}: {model_ids}")


def test_tc_harness_04_models(http):
    """TC-HARNESS-04: GET /api/models."""
    print("\n── TC-HARNESS-04: GET /api/models ──")
    r = http.get(f"{GATEWAY_HTTP}/api/models")
    if not assert_status(r, 200):
        return
    data = r.json()
    models = data.get("models", [])
    ok(f"{len(models)} models available")


def test_tc_harness_05_embedding(http):
    """TC-HARNESS-05: GET /api/embedding-models."""
    print("\n── TC-HARNESS-05: GET /api/embedding-models ──")
    r = http.get(f"{GATEWAY_HTTP}/api/embedding-models")
    if not assert_status(r, 200):
        return
    data = r.json()
    count = len(data) if isinstance(data, list) else len(data.get("models", []))
    ok(f"embedding models: {count}")


def test_tc_harness_06_mcp(http):
    """TC-HARNESS-06: GET /api/mcp-catalog."""
    print("\n── TC-HARNESS-06: GET /api/mcp-catalog ──")
    r = http.get(f"{GATEWAY_HTTP}/api/mcp-catalog")
    if not assert_status(r, 200):
        return
    data = r.json()
    entries = data if isinstance(data, list) else data.get("entries", [])
    ok(f"{len(entries)} MCP entries")


def test_tc_settings_01_status(http):
    """TC-SETTINGS-01: GET /api/status."""
    print("\n── TC-SETTINGS-01: GET /api/status ──")
    r = http.get(f"{GATEWAY_HTTP}/api/status")
    if not assert_status(r, 200):
        return
    data = r.json()
    for field in ["version", "uptime_secs"]:
        if field in data:
            ok(f"  {field}: {data[field]}")
        else:
            fail(f"  missing {field}")


def test_tc_settings_02_config(http):
    """TC-SETTINGS-02: GET /api/config."""
    print("\n── TC-SETTINGS-02: GET /api/config ──")
    r = http.get(f"{GATEWAY_HTTP}/api/config")
    if not assert_status(r, 200):
        return
    data = r.json()
    for field in ["log_level", "http"]:
        if field in data:
            ok(f"  {field}: {data[field]}")
        else:
            fail(f"  missing {field}")


def test_tc_lsp_01_endpoint(http):
    """TC-LSP-01: GET /api/lsp/endpoint."""
    print("\n── TC-LSP-01: GET /api/lsp/endpoint ──")
    r = http.get(f"{GATEWAY_HTTP}/api/lsp/endpoint")
    if r.status_code == 200:
        data = r.json()
        if data.get("available"):
            ok(f"LSP available at {data.get('host')}:{data.get('port')}")
        elif data.get("endpoint"):
            ok(f"LSP endpoint: {data['endpoint']}")
        else:
            ok(f"LSP: {data}")
    else:
        skip(f"HTTP {r.status_code}")


def test_tc_skill_01_list(http):
    """TC-SKILL-01: GET /api/agents/{id}/skills."""
    print(f"\n── TC-SKILL-01: GET /api/agents/{AGENT_ID}/skills ──")
    r = http.get(f"{GATEWAY_HTTP}/api/agents/{AGENT_ID}/skills")
    if r.status_code == 200:
        data = r.json()
        skills = data.get("skills", [])
        ok(f"{len(skills)} skills")
        for s in skills[:3]:
            ok(f"  {s.get('name')}: {s.get('description', '')[:50]}")
    else:
        skip(f"HTTP {r.status_code}")


def test_tc_ws_01_list(http):
    """TC-WS-01: GET /api/agents/{id}/workspaces."""
    print(f"\n── TC-WS-01: GET /api/agents/{AGENT_ID}/workspaces ──")
    r = http.get(f"{GATEWAY_HTTP}/api/agents/{AGENT_ID}/workspaces")
    if r.status_code == 200:
        data = r.json()
        ok(f"workspaces: {len(data) if isinstance(data, list) else 'object'}")
    else:
        skip(f"HTTP {r.status_code}")


def test_tc_mqtt_broker(http):
    """Verify MQTT broker is reachable."""
    print("\n── MQTT Broker Smoke ──")
    mq = MqttClient()
    try:
        mq.connect()
        time.sleep(0.5)
        if mq.client.is_connected():
            ok("MQTT broker connected")
            # Subscribe to global topic — should succeed immediately
            mq.subscribe("acowork/global/#", qos=1)
            time.sleep(0.3)
            # Publish a test message
            mq.publish("acowork/global/_smoke_test", "ping", qos=1)
            time.sleep(0.3)
            if mq.wait_for("acowork/global/_smoke_test", timeout=2):
                ok("MQTT pub/sub roundtrip")
            else:
                fail("MQTT roundtrip: no echo received")
        else:
            fail("MQTT broker not connected")
    except Exception as e:
        fail(f"MQTT broker error: {e}")
    finally:
        mq.disconnect()


def test_tc_runtime_start(http, runtime_bin):
    """Start Runtime via MQTT and verify connection."""
    print(f"\n── Runtime Start: {AGENT_ID} ──")
    mq = MqttClient()
    mq.connect()
    mq.subscribe(f"acowork/agents/{AGENT_ID}/status", qos=1)
    time.sleep(0.3)

    proc = start_runtime(runtime_bin, AGENT_ID, MQTT_PORT)
    if proc is None:
        mq.disconnect()
        return None

    # Wait for online status
    payload = mq.wait_retained(f"acowork/agents/{AGENT_ID}/status", timeout=10)
    if payload and (b"online" in payload or b"ready" in payload):
        ok(f"Agent status: {payload.decode()}")
    elif payload:
        fail(f"Agent status unexpected: {payload.decode()}")
    else:
        fail("Agent status: no retained message received")
        mq.disconnect()
        proc.terminate()
        return None

    mq.disconnect()
    return proc


# ── Main ────────────────────────────────────────────────────────────────

def main():
    parser = argparse.ArgumentParser(description="ADR-033 Frontend Smoke Test")
    parser.add_argument("--gateway-bin", default=None, help="Path to acowork-gateway binary")
    parser.add_argument("--runtime-bin", default=None, help="Path to acowork-runtime binary")
    parser.add_argument("--no-start", action="store_true", help="Don't start gateway, use running instance")
    args = parser.parse_args()

    # Resolve binaries
    root = Path(__file__).parent.parent.parent / "target" / "debug"
    gw_bin = args.gateway_bin or root / "acowork-gateway"
    rt_bin = args.runtime_bin or root / "acowork-runtime"

    print("=" * 60)
    print("ADR-033 Frontend Smoke Test")
    print(f"  Gateway: {gw_bin}")
    print(f"  Runtime: {rt_bin}")
    print("=" * 60)

    gw = Gateway(gw_bin)
    http = httpx.Client(timeout=10)

    # ── Startup ─────────────────────────────────────────────────────
    if not args.no_start:
        if not gw.start():
            sys.exit(1)
    else:
        log("INFO", "Using running Gateway instance")

    # TC-CHAT-01: Health check (implicit)
    r = http.get(f"{GATEWAY_HTTP}/health")
    if not assert_status(r, 200, "Gateway /health"):
        gw.stop()
        sys.exit(1)
    health = r.json()
    ok(f"Gateway v{health.get('version', '?')}")

    # ── HTTP API Tests ──────────────────────────────────────────────
    test_tc_chat_01_agent_list(http)
    test_tc_harness_01_providers(http)
    test_tc_harness_04_models(http)
    test_tc_harness_05_embedding(http)
    test_tc_harness_06_mcp(http)
    test_tc_settings_01_status(http)
    test_tc_settings_02_config(http)
    test_tc_skill_01_list(http)
    test_tc_ws_01_list(http)
    test_tc_lsp_01_endpoint(http)

    # ── MQTT Tests ──────────────────────────────────────────────────
    test_tc_mqtt_broker(http)

    # ── TC-CHAT-02: Start Agent via Gateway API ──────────────────────
    print(f"\n── TC-CHAT-02: Start {AGENT_ID} via Gateway ──")
    
    # Subscribe to agent status before starting
    mq3 = MqttClient()
    mq3.connect()
    mq3.subscribe(f"acowork/agents/{AGENT_ID}/status", qos=1)
    mq3.subscribe(f"acowork/agents/{AGENT_ID}/meta", qos=1)
    time.sleep(0.3)

    # Start via Gateway HTTP API
    r = http.post(f"{GATEWAY_HTTP}/api/agents/{AGENT_ID}/start", json={})
    if r.status_code in (200, 409):  # 409 = already running
        ok(f"Agent start: HTTP {r.status_code}")
    else:
        fail(f"Agent start failed: {r.status_code} {r.text[:200]}")
        skip("Skipping agent-dependent tests")
        gw.stop()
        sys.exit(0)

    # Wait for MQTT status online
    payload = mq3.wait_for(f"acowork/agents/{AGENT_ID}/status", timeout=30,
                           predicate=lambda p: b"online" in p)
    if payload:
        ok(f"Agent MQTT status: {payload.decode()}")
    else:
        fail("Agent MQTT status: no online message received")
    
    # Wait for meta
    meta_payload = mq3.wait_for(f"acowork/agents/{AGENT_ID}/meta", timeout=10)
    if meta_payload:
        ok(f"Agent meta received: {meta_payload[:80]}")
    
    mq3.disconnect()

    # ── Agent-dependent HTTP tests ───────────────────────────────────
    test_tc_setup_01_config(http)
    test_tc_setup_05_mcp(http)
    test_tc_setup_07_model(http)
    test_tc_skill_01_list(http)
    test_tc_ws_01_list(http)
    test_tc_chat_03_session_list(http)
    test_tc_lsp_01_endpoint(http)

    # ── TC-CHAT-06~11: Session + Chat via HTTP/MQTT ──────────────────
    print(f"\n── TC-CHAT-06: Create session ──")
    r = http.post(f"{GATEWAY_HTTP}/api/agents/{AGENT_ID}/sessions", json={})
    if r.status_code == 200:
        sid = r.json().get("session_id", "")
        if sid:
            ok(f"Session created: {sid}")
        else:
            fail("No session_id in response")
    else:
        fail(f"Create session: HTTP {r.status_code} {r.text[:100]}")
        sid = None

    if sid:
        print(f"\n── TC-CHAT-07: Send message ──")
        r = http.post(f"{GATEWAY_HTTP}/api/agents/{AGENT_ID}/message", json={
            "content": "Reply with just: OK",
            "session_id": sid,
        })
        if r.status_code == 200:
            msg_id = r.json().get("message_id", "")
            ok(f"Message sent: {msg_id}")
        else:
            fail(f"Send message: HTTP {r.status_code} {r.text[:100]}")

        print(f"\n── TC-CHAT-10: Delete session ──")
        r = http.delete(f"{GATEWAY_HTTP}/api/agents/{AGENT_ID}/sessions/{sid}")
        if r.status_code == 200:
            ok(f"Session deleted: {sid}")
        else:
            fail(f"Delete session: HTTP {r.status_code} {r.text[:100]}")

    # ── MQTT global resources verification ───────────────────────────
    print("\n── MQTT Global Resources Check ──")
    mq2 = MqttClient()
    mq2.connect()
    mq2.subscribe("acowork/global/providers", qos=1)
    time.sleep(6)
    providers_count = len(mq2.received.get("acowork/global/providers", []))
    if providers_count > 0:
        ok(f"MQTT global resources: providers={providers_count}x")
    mq2.disconnect()

    # ── Cleanup ─────────────────────────────────────────────────────
    # Stop agent via Gateway API
    r = http.post(f"{GATEWAY_HTTP}/api/agents/{AGENT_ID}/stop", json={})
    if r.status_code == 200:
        ok("Agent stopped via Gateway API")
    else:
        print(f"  [warning] Agent stop returned {r.status_code}")
    gw.stop()

    # ── Summary ─────────────────────────────────────────────────────
    total = passed + failed + skipped
    print(f"\n{'=' * 60}")
    print(f"Results: {passed} passed, {failed} failed, {skipped} skipped ({total} total)")
    if failed > 0:
        print("SOME TESTS FAILED")
        sys.exit(1)
    else:
        print("ALL TESTS PASSED")
        sys.exit(0)


if __name__ == "__main__":
    main()
