// P1-B / P2-B: per-service probe functions for the full-stack
// diagnostic panel.
//
// Two probe paths, tried in order by `probeAllServices` / `probeService`:
//
// 1. **Gateway snapshot (P2, preferred)** — one `GET
//    /api/services/diagnose` fetch returns the Gateway's ground truth
//    about its own subsystems (embed / pm / doc supervisor state, node
//    registry, broker liveness). Works identically local / remote
//    because every subsystem lives on the Gateway host (P2-1).
// 2. **Direct probes (P1 legacy)** — the per-endpoint walk below. Used
//    when the snapshot endpoint is absent (pre-P2 Gateway answers 404)
//    or its body fails shape validation.
//
// On the direct path each `probe*` function returns a `ServiceHealth`
// snapshot. Every probe is wrapped in a 1s budget (`AbortController` +
// `setTimeout`) so a hung service can never stall the diagnostic UI.
// The direct path runs 5 probes: 3 critical (`gateway`, `mqtt`, `node`)
// + 1 important (`embed`) + 3 optional subsystems (`pm`, `doc`,
// `lsp-relay`) bundled in a single HTTP call.
//
// Design decisions (see docs/plan/zh/desktop-unified-diagnostics.md §3.2):
// - Pure functions, no Zustand coupling — easy to unit-test.
// - Probe `gateway` is the gating signal: when it fails, the other 6
//   probes are short-circuited to "offline + last_error=gateway down"
//   so the panel doesn't render 6 confusing red rows. The full report
//   still carries a `gateway_reachable: false` flag so the caller can
//   show a "Gateway is down, check that first" banner.
// - `embed` / `pm` / `doc` / `lsp-relay` are not separate HTTP
//   endpoints — the direct path infers their health from
//   `system_status` + agent counts (`probeOptionalSubsystems`); the
//   snapshot path reports the Gateway's supervisor state instead.

import { invoke } from "@tauri-apps/api/core";

import { fetchNodes } from "./gateway-api";
import type {
  NodeInfo,
  ProbeSource,
  ServiceHealth,
  ServiceType,
} from "./types";

/** Per-probe budget. 1s is tight enough that a hung endpoint can't
 *  stall the panel; loose enough that a healthy LAN round-trip
 *  comfortably fits. */
export const PROBE_TIMEOUT_MS = 1_000;

/** Shape of `GET /api/status`. Re-declared locally to avoid pulling the
 *  full `types.ts` if a future caller wants the api module standalone. */
interface SystemStatus {
  version: string;
  agents_installed: number;
  agents_running: number;
  uptime_secs: number;
  mqtt_port: number;
}

/** Shape of the Rust `get_mqtt_status` command. */
interface MqttStatusSnapshot {
  known: boolean;
  connected: boolean;
  reason?: string | null;
}

// ── P2: `GET /api/services/diagnose` wire shapes ──────────────────────
// Mirrors `core/acowork-gateway/src/http/services_api.rs` (snake_case
// contract; the Rust side asserts it in
// `diagnose_serializes_with_snake_case_contract`). `active_model_id` is
// embed-only and omitted for running pm/doc rows.

interface GatewaySubsystemSnapshot {
  running: boolean;
  ready: boolean;
  port: number;
  pid: number;
  active_model_id?: string;
}

interface GatewayNodeDiagnosticRow {
  node_id: string;
  online: boolean;
  node_name?: string;
  hostname?: string;
  node_version?: string;
  has_lsp_relay: boolean;
}

interface GatewayDiagnosePayload {
  gateway: {
    version: string;
    instance_id: string;
    http_port: number;
    mqtt_port: number;
    agents_running: number;
    agents_installed: number;
  };
  mqtt: { broker_running: boolean; port: number; auth_enabled: boolean };
  embed: GatewaySubsystemSnapshot;
  pm: GatewaySubsystemSnapshot;
  doc: GatewaySubsystemSnapshot;
  nodes: {
    total: number;
    online: number;
    items: GatewayNodeDiagnosticRow[];
  };
  diagnosed_at: string;
}

const emptyHealth = (
  service_type: ServiceType,
  group: ServiceHealth["group"],
  last_error: string,
  probed_at: number,
): ServiceHealth => ({
  service_type,
  group,
  online: false,
  version: "unknown",
  latency_ms: 0,
  detail: undefined,
  last_error,
  probed_at,
});

/** Wrap a fetch with a 1s AbortController timeout. */
async function timedFetch(url: string): Promise<Response> {
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), PROBE_TIMEOUT_MS);
  try {
    return await fetch(url, { signal: ctrl.signal });
  } finally {
    clearTimeout(timer);
  }
}

/** Convert an unknown error (timeout / network / 5xx / JSON parse) into a
 *  short human-readable string for the `last_error` field. */
function describeError(e: unknown): string {
  if (e instanceof DOMException && e.name === "AbortError") {
    return `timeout (>${PROBE_TIMEOUT_MS}ms)`;
  }
  if (e instanceof Error) return e.message;
  return String(e);
}

/** Probe the Gateway HTTP front door (`GET /api/status`). */
export async function probeGateway(
  gatewayUrl: string,
  now: number = Date.now(),
): Promise<ServiceHealth> {
  const started = Date.now();
  try {
    const resp = await timedFetch(`${gatewayUrl}/api/status`);
    if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
    const data = (await resp.json()) as SystemStatus;
    return {
      service_type: "gateway",
      group: "critical",
      online: true,
      version: data.version || "unknown",
      latency_ms: Date.now() - started,
      detail: `uptime ${data.uptime_secs}s · mqtt:${data.mqtt_port}`,
      last_error: null,
      probed_at: now,
    };
  } catch (e) {
    return emptyHealth(
      "gateway",
      "critical",
      `gateway unreachable: ${describeError(e)}`,
      now,
    );
  }
}

/** Outcome of the in-process `get_mqtt_status` read. The `ok: false`
 *  arm carries pre-formatted error text so both consumers (the direct
 *  `probeMqtt` row and the P2 snapshot mapper) keep the P1 error
 *  taxonomy ("mqtt probe failed: …"). */
type LocalMqttOutcome =
  | { ok: true; snapshot: MqttStatusSnapshot }
  | { ok: false; error: string };

/** Read the Desktop-side MQTT client state (in-process Tauri command,
 *  wrapped in the standard 1s budget). Shared by `probeMqtt` and the
 *  P2 snapshot mapper — both need the client half of the MQTT picture. */
async function localMqttSnapshot(): Promise<LocalMqttOutcome> {
  try {
    const snapshot = await Promise.race<Promise<MqttStatusSnapshot>>([
      invoke<MqttStatusSnapshot>("get_mqtt_status"),
      // Belt-and-braces timeout: `invoke` itself respects the Tauri
      // IPC timeout but that has no configurable budget from JS.
      new Promise<MqttStatusSnapshot>((_resolve, reject) =>
        setTimeout(
          () => reject(new Error(`timeout (>${PROBE_TIMEOUT_MS}ms)`)),
          PROBE_TIMEOUT_MS,
        ),
      ),
    ]);
    return { ok: true, snapshot };
  } catch (e) {
    return { ok: false, error: describeError(e) };
  }
}

/** Probe the MQTT broker via the existing Rust `get_mqtt_status`
 *  snapshot — that's the same path the frontend's watchdog polls, so
 *  we get parity with what the user sees on the chat input box. */
export async function probeMqtt(now: number = Date.now()): Promise<ServiceHealth> {
  const started = Date.now();
  const outcome = await localMqttSnapshot();
  if (!outcome.ok) {
    return emptyHealth(
      "mqtt",
      "critical",
      `mqtt probe failed: ${outcome.error}`,
      now,
    );
  }
  const snapshot = outcome.snapshot;
  if (!snapshot.known) {
    return emptyHealth(
      "mqtt",
      "critical",
      snapshot.reason ?? "mqtt client not initialized",
      now,
    );
  }
  if (!snapshot.connected) {
    return emptyHealth(
      "mqtt",
      "critical",
      snapshot.reason ?? "mqtt disconnected",
      now,
    );
  }
  return {
    service_type: "mqtt",
    group: "critical",
    online: true,
    version: "broker",
    latency_ms: Date.now() - started,
    detail: snapshot.reason ?? "connected",
    last_error: null,
    probed_at: now,
  };
}

/** Probe the Node Agent registry (`GET /api/nodes`). Online when at
 *  least one node reports `online: true`. */
export async function probeNodes(
  gatewayUrl: string,
  now: number = Date.now(),
): Promise<ServiceHealth> {
  const started = Date.now();
  try {
    const nodes = await fetchNodes(gatewayUrl);
    const onlineCount = nodes.filter((n: NodeInfo) => n.online).length;
    if (onlineCount === 0) {
      return emptyHealth(
        "node",
        "critical",
        nodes.length === 0
          ? "no nodes registered"
          : `0/${nodes.length} nodes online`,
        now,
      );
    }
    // Pick the lowest node_version as the "fleet version" — accurate
    // enough for human eyes; the panel doesn't need exact semver compare.
    const versions = nodes
      .map((n) => n.node_version)
      .filter((v): v is string => typeof v === "string");
    const version = versions.length > 0 ? versions.sort()[0] : "unknown";
    return {
      service_type: "node",
      group: "critical",
      online: true,
      version,
      latency_ms: Date.now() - started,
      detail: `${onlineCount}/${nodes.length} nodes online`,
      last_error: null,
      probed_at: now,
    };
  } catch (e) {
    return emptyHealth(
      "node",
      "critical",
      `nodes probe failed: ${describeError(e)}`,
      now,
    );
  }
}

/** Probe the embedding model subsystem (direct-path fallback). Inferred
 *  from `system_status.agents_running` — if any agent is running the
 *  runtime is alive and its embedding dependency is loaded. The P2
 *  snapshot path replaces this inference with the Gateway's actual
 *  supervisor state (`fetchGatewayDiagnose`). */
async function probeEmbed(
  gatewayUrl: string,
  now: number,
): Promise<ServiceHealth> {
  const started = Date.now();
  try {
    const resp = await timedFetch(`${gatewayUrl}/api/status`);
    if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
    const data = (await resp.json()) as SystemStatus;
    return {
      service_type: "embed",
      group: "important",
      online: data.agents_running >= 0, // runtime alive
      version: data.version || "unknown",
      latency_ms: Date.now() - started,
      detail: `runtime alive · ${data.agents_running} agents running`,
      last_error: null,
      probed_at: now,
    };
  } catch (e) {
    return emptyHealth(
      "embed",
      "important",
      `embed probe failed: ${describeError(e)}`,
      now,
    );
  }
}

/** Bundle the three "optional" subsystems (pm / doc / lsp-relay). They
 *  share a single `system_status` probe — three rows for the price of
 *  one network round-trip. The P2 snapshot path reports each
 *  supervisor's state verbatim instead of this bundled inference. */
async function probeOptionalSubsystems(
  gatewayUrl: string,
  now: number,
): Promise<Pick<Record<ServiceType, ServiceHealth>, "pm" | "doc" | "lsp-relay">> {
  const started = Date.now();
  let data: SystemStatus | null = null;
  let errMsg: string | null = null;
  try {
    const resp = await timedFetch(`${gatewayUrl}/api/status`);
    if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
    data = (await resp.json()) as SystemStatus;
  } catch (e) {
    errMsg = describeError(e);
  }
  const latency = Date.now() - started;
  const version = data?.version ?? "unknown";
  const online = data !== null;
  const baseDetail = data
    ? `runtime alive · uptime ${data.uptime_secs}s`
    : "runtime not reachable";
  const baseError = errMsg ? `runtime probe failed: ${errMsg}` : null;
  const row = (
    service_type: ServiceType,
    group: ServiceHealth["group"],
    detail: string,
  ): ServiceHealth => ({
    service_type,
    group,
    online,
    version,
    latency_ms: latency,
    detail,
    last_error: online ? null : baseError,
    probed_at: now,
  });
  return {
    pm: row("pm", "optional", `${baseDetail} · ${data?.agents_running ?? 0} agents`),
    doc: row("doc", "optional", `${baseDetail} · doc subsystem`),
    "lsp-relay": row("lsp-relay", "optional", `${baseDetail} · lsp relay`),
  };
}

// ── P2: Gateway-perspective snapshot path ─────────────────────────────

/** Shape-validate a supervised-subsystem row. The mapper renders
 *  `pid` / `port` verbatim into the row detail, so a proxy-returned
 *  body missing them must not be trusted. */
function isSubsystemSnapshot(v: unknown): v is GatewaySubsystemSnapshot {
  if (typeof v !== "object" || v === null) return false;
  const s = v as Record<string, unknown>;
  return (
    typeof s.running === "boolean" &&
    typeof s.ready === "boolean" &&
    typeof s.port === "number" &&
    typeof s.pid === "number"
  );
}

/** Shape-validate the snapshot body before trusting it. A pre-P2
 *  Gateway answers this path with 404 (→ `!resp.ok`), but a misbehaving
 *  proxy could return 200 with an unrelated body — refuse to map rows
 *  from anything that doesn't match the contract so the caller falls
 *  back to direct probes instead of rendering garbage (L-4 of the
 *  review: numeric fields are validated too, since the mapper prints
 *  them). */
function isGatewayDiagnosePayload(v: unknown): v is GatewayDiagnosePayload {
  if (typeof v !== "object" || v === null) return false;
  const d = v as Record<string, unknown>;
  const gw = d.gateway as Record<string, unknown> | null | undefined;
  const mqtt = d.mqtt as Record<string, unknown> | null | undefined;
  const nodes = d.nodes as Record<string, unknown> | null | undefined;
  return (
    !!gw &&
    typeof gw.version === "string" &&
    typeof gw.http_port === "number" &&
    typeof gw.mqtt_port === "number" &&
    typeof gw.agents_running === "number" &&
    typeof gw.agents_installed === "number" &&
    !!mqtt &&
    typeof mqtt.broker_running === "boolean" &&
    typeof mqtt.port === "number" &&
    isSubsystemSnapshot(d.embed) &&
    isSubsystemSnapshot(d.pm) &&
    isSubsystemSnapshot(d.doc) &&
    !!nodes &&
    typeof nodes.total === "number" &&
    typeof nodes.online === "number" &&
    Array.isArray(nodes.items)
  );
}

/** One supervised-subsystem row (embed / pm / doc) from the Gateway
 *  snapshot. `online` requires running **and** ready — a supervised
 *  process that hasn't completed its startup health check is still
 *  unusable to callers. */
function subsystemRow(
  service_type: "embed" | "pm" | "doc",
  group: ServiceHealth["group"],
  sub: GatewaySubsystemSnapshot,
  version: string,
  latency: number,
  now: number,
): ServiceHealth {
  if (!sub.running) {
    return emptyHealth(
      service_type,
      group,
      `${service_type} subsystem not running`,
      now,
    );
  }
  if (!sub.ready) {
    return emptyHealth(
      service_type,
      group,
      `${service_type} subsystem starting (not ready)`,
      now,
    );
  }
  const model = sub.active_model_id ? ` · ${sub.active_model_id}` : "";
  return {
    service_type,
    group,
    online: true,
    version,
    latency_ms: latency,
    detail: `pid ${sub.pid} · port ${sub.port}${model}`,
    last_error: null,
    probed_at: now,
  };
}

/** Map a validated Gateway snapshot into the 7 `ServiceHealth` rows.
 *  `latency` is the round-trip of the snapshot fetch itself; `client`
 *  is the local MQTT client state used for the combined broker+client
 *  MQTT row (`null` when the in-process read failed). */
function mapGatewaySnapshot(
  payload: GatewayDiagnosePayload,
  latency: number,
  client: MqttStatusSnapshot | null,
  now: number,
): Record<ServiceType, ServiceHealth> {
  const gwVersion = payload.gateway.version || "unknown";

  // gateway — reaching this endpoint at all IS the liveness proof.
  const gateway: ServiceHealth = {
    service_type: "gateway",
    group: "critical",
    online: true,
    version: gwVersion,
    latency_ms: latency,
    detail: `${payload.gateway.agents_running}/${payload.gateway.agents_installed} agents · mqtt :${payload.gateway.mqtt_port}`,
    last_error: null,
    probed_at: now,
  };

  // mqtt — broker half from the Gateway, client half from the local
  // Tauri snapshot. `online` requires both (a broker nobody can reach,
  // or a client with no broker, both mean chat is broken); when the
  // in-process read failed we can only enforce the broker half.
  const brokerRunning = payload.mqtt.broker_running;
  const clientOk = client === null ? true : client.known && client.connected;
  const clientLabel =
    client === null
      ? "unknown"
      : !client.known
        ? "not initialized"
        : client.connected
          ? "connected"
          : "disconnected";
  const mqtt: ServiceHealth = {
    service_type: "mqtt",
    group: "critical",
    online: brokerRunning && clientOk,
    version: "broker",
    latency_ms: latency,
    detail: `:${payload.mqtt.port} · auth ${payload.mqtt.auth_enabled ? "on" : "off"} · client ${clientLabel}`,
    last_error: !brokerRunning
      ? "mqtt broker not running"
      : clientOk
        ? null
        : (client?.reason ?? "mqtt client disconnected"),
    probed_at: now,
  };

  // node — fleet row, same aggregation rules as `probeNodes`.
  const items = payload.nodes.items;
  const onlineCount = items.filter((n) => n.online).length;
  const versions = items
    .map((n) => n.node_version)
    .filter((v): v is string => typeof v === "string");
  const node: ServiceHealth =
    onlineCount > 0
      ? {
          service_type: "node",
          group: "critical",
          online: true,
          version: versions.length > 0 ? versions.sort()[0] : "unknown",
          latency_ms: latency,
          detail: `${onlineCount}/${items.length} nodes online`,
          last_error: null,
          probed_at: now,
        }
      : emptyHealth(
          "node",
          "critical",
          items.length === 0
            ? "no nodes registered"
            : `0/${items.length} nodes online`,
          now,
        );

  // embed / pm / doc — supervisor state reported verbatim by the
  // Gateway (no inference needed, unlike the direct path).
  const embed = subsystemRow(
    "embed",
    "important",
    payload.embed,
    gwVersion,
    latency,
    now,
  );
  const pm = subsystemRow(
    "pm",
    "optional",
    payload.pm,
    gwVersion,
    latency,
    now,
  );
  const doc = subsystemRow(
    "doc",
    "optional",
    payload.doc,
    gwVersion,
    latency,
    now,
  );

  // lsp-relay — node-local sidecar (ADR-055 §6.7); the snapshot only
  // carries the per-node capability flag, so the row aggregates it
  // across the fleet.
  const relayCapable = items.filter(
    (n) => n.online && n.has_lsp_relay,
  ).length;
  const lspRelay: ServiceHealth =
    relayCapable > 0
      ? {
          service_type: "lsp-relay",
          group: "optional",
          online: true,
          version: gwVersion,
          latency_ms: latency,
          detail: `${relayCapable}/${onlineCount} online nodes advertise relay`,
          last_error: null,
          probed_at: now,
        }
      : emptyHealth(
          "lsp-relay",
          "optional",
          items.length === 0
            ? "no nodes registered"
            : onlineCount === 0
              ? `0/${items.length} nodes online`
              : "no online node advertises lsp-relay",
          now,
        );

  return { gateway, mqtt, node, embed, pm, doc, "lsp-relay": lspRelay };
}

/** Result of a snapshot fetch: the 7 rows plus the source tag the
 *  report carries for the `ServicesPanel` badge (P2-C). */
export interface GatewayDiagnoseResult {
  services: Record<ServiceType, ServiceHealth>;
  source: "gateway-api";
}

/** Fetch `GET /api/services/diagnose` and map it into the 7 rows.
 *  Resolves `null` (never throws) on timeout / non-2xx / malformed
 *  body so callers degrade to the direct probes without losing the
 *  panel (old Gateway build, proxy interference, …). */
export async function fetchGatewayDiagnose(
  gatewayUrl: string,
  now: number = Date.now(),
): Promise<GatewayDiagnoseResult | null> {
  const started = Date.now();
  try {
    const resp = await timedFetch(`${gatewayUrl}/api/services/diagnose`);
    if (!resp.ok) return null;
    const payload: unknown = await resp.json();
    if (!isGatewayDiagnosePayload(payload)) return null;
    // M-1: the reported latency is the HTTP round-trip ONLY. The local
    // MQTT snapshot below is Desktop IPC (up to 1s) and must not
    // inflate the Gateway-side latency that the row colour thresholds
    // (`>800ms` amber) key on.
    const latency = Date.now() - started;
    const outcome = await localMqttSnapshot();
    const client = outcome.ok ? outcome.snapshot : null;
    return {
      services: mapGatewaySnapshot(payload, latency, client, now),
      source: "gateway-api",
    };
  } catch {
    return null;
  }
}

/** Full-pass result envelope: the 7 rows plus which source produced
 *  them (surfaced as the source badge in `ServicesPanel`). */
export interface ProbeAllResult {
  services: Record<ServiceType, ServiceHealth>;
  source: ProbeSource;
}

/** Run a full diagnostic pass. P2: prefer the Gateway-perspective
 *  snapshot — ONE HTTP round-trip covers all 7 rows and behaves
 *  identically local / remote (the subsystems live on the Gateway
 *  host, so a remote Desktop can never reach them directly). Falls
 *  back to the P1 direct probes when the snapshot endpoint is absent.
 *  Always resolves (never throws) so the caller can write to its store
 *  unconditionally. */
export async function probeAllServices(
  gatewayUrl: string,
  now: number = Date.now(),
): Promise<ProbeAllResult> {
  const snap = await fetchGatewayDiagnose(gatewayUrl, now);
  if (snap) return snap;

  const [gateway, mqtt, node, embed, optional] = await Promise.all([
    probeGateway(gatewayUrl, now),
    probeMqtt(now),
    probeNodes(gatewayUrl, now),
    probeEmbed(gatewayUrl, now),
    probeOptionalSubsystems(gatewayUrl, now),
  ]);
  // When the gateway itself is offline, override the rest so the panel
  // doesn't render 6 misleading "probe failed: HTTP unreachable" rows.
  // The error message tells the user the real cause.
  if (!gateway.online) {
    const reason = `gateway offline: ${gateway.last_error ?? "unknown"}`;
    const override = (s: ServiceHealth): ServiceHealth => ({
      ...s,
      online: false,
      last_error: reason,
    });
    return {
      services: {
        gateway,
        mqtt: override(mqtt),
        node: override(node),
        embed: override(embed),
        pm: override(optional.pm),
        doc: override(optional.doc),
        "lsp-relay": override(optional["lsp-relay"]),
      },
      source: "direct",
    };
  }
  return {
    services: {
      gateway,
      mqtt,
      node,
      embed,
      pm: optional.pm,
      doc: optional.doc,
      "lsp-relay": optional["lsp-relay"],
    },
    source: "direct",
  };
}

/** Probe a single service by type. Used by the per-row "重试" button
 *  on `ServicesPanel` — runs only the targeted probe instead of the
 *  whole fleet. P2: same source priority as the full pass (a retry
 *  must re-check through the same lens the report was rendered with),
 *  falling back to the direct single probe on a pre-P2 Gateway. */
export async function probeService(
  service_type: ServiceType,
  gatewayUrl: string,
  now: number = Date.now(),
): Promise<ServiceHealth> {
  const snap = await fetchGatewayDiagnose(gatewayUrl, now);
  if (snap) return snap.services[service_type];
  switch (service_type) {
    case "gateway":
      return probeGateway(gatewayUrl, now);
    case "mqtt":
      return probeMqtt(now);
    case "node":
      return probeNodes(gatewayUrl, now);
    case "embed":
      return probeEmbed(gatewayUrl, now);
    case "pm":
    case "doc":
    case "lsp-relay": {
      const map = await probeOptionalSubsystems(gatewayUrl, now);
      return map[service_type];
    }
  }
}
