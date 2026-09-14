/**
 * servicesStore tests — P1-G.
 *
 * Coverage focus:
 *   - `diagnose()` populates `report` and clears `loading`
 *   - `diagnose()` propagates the probe `source` tag into the report (P2)
 *   - `diagnose()` re-entrancy guard: while one pass is in flight,
 *     a second call is a no-op (no double `Promise.all`)
 *   - `probe(service_type)` merges a single row into the existing report
 *   - `probe(service_type)` does not create a report if none exists yet
 *   - `reset()` restores the initial state
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

// Mock `servicesApi` so the store exercises pure logic, not real
// HTTP / Tauri calls.
vi.mock("../lib/servicesApi", () => ({
  probeAllServices: vi.fn(),
  probeService: vi.fn(),
}));

vi.mock("../lib/config", () => ({
  getGatewayUrl: () => "http://gw",
}));

import { probeAllServices, probeService } from "../lib/servicesApi";
import { useServicesStore } from "./servicesStore";
import type { ServiceHealth } from "../lib/types";

const mockedProbeAll = vi.mocked(probeAllServices);
const mockedProbeOne = vi.mocked(probeService);

function mkRow(overrides: Partial<ServiceHealth> = {}): ServiceHealth {
  return {
    service_type: "gateway",
    group: "critical",
    online: true,
    version: "0.9.1",
    latency_ms: 12,
    detail: "ok",
    last_error: null,
    probed_at: 0,
    ...overrides,
  };
}

const FULL_REPORT: Record<string, ServiceHealth> = {
  gateway: mkRow({ service_type: "gateway" }),
  mqtt: mkRow({ service_type: "mqtt" }),
  node: mkRow({ service_type: "node" }),
  embed: mkRow({ service_type: "embed" }),
  pm: mkRow({ service_type: "pm" }),
  doc: mkRow({ service_type: "doc" }),
  "lsp-relay": mkRow({ service_type: "lsp-relay" }),
};

beforeEach(() => {
  useServicesStore.getState().reset();
  mockedProbeAll.mockReset();
  mockedProbeOne.mockReset();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("servicesStore.diagnose", () => {
  it("populates the report and clears loading on success", async () => {
    mockedProbeAll.mockResolvedValueOnce({
      services: FULL_REPORT,
      source: "gateway-api",
    } as never);
    await useServicesStore.getState().diagnose();

    const s = useServicesStore.getState();
    expect(s.loading).toBe(false);
    expect(s.report).not.toBeNull();
    expect(s.report?.services.gateway.version).toBe("0.9.1");
    expect(s.report?.gateway_reachable).toBe(true);
    // P2: the probe source tag must survive the store write — it drives
    // the "via Gateway snapshot / direct probes" badge on the panel.
    expect(s.report?.source).toBe("gateway-api");
    expect(s.lastProbeAt).not.toBeNull();
    expect(s.lastError).toBeNull();
  });

  it("flags gateway_reachable=false when the gateway row is offline", async () => {
    const down: Record<string, ServiceHealth> = {
      ...FULL_REPORT,
      gateway: mkRow({
        service_type: "gateway",
        online: false,
        version: "unknown",
        last_error: "ECONNREFUSED",
      }),
    };
    mockedProbeAll.mockResolvedValueOnce({
      services: down,
      source: "direct",
    } as never);
    await useServicesStore.getState().diagnose();
    expect(useServicesStore.getState().report?.gateway_reachable).toBe(false);
  });

  it("is re-entrant: a second concurrent diagnose() is a no-op", async () => {
    // First call hangs; second call should be skipped while the first
    // is still in flight.
    let resolveFirst!: (v: unknown) => void;
    mockedProbeAll.mockReturnValueOnce(
      new Promise((resolve) => {
        resolveFirst = resolve as never;
      }),
    );

    const first = useServicesStore.getState().diagnose();
    expect(useServicesStore.getState().loading).toBe(true);
    expect(mockedProbeAll).toHaveBeenCalledTimes(1);

    // Second call: loading is true → guard should skip probeAllServices.
    await useServicesStore.getState().diagnose();
    expect(mockedProbeAll).toHaveBeenCalledTimes(1);

    resolveFirst({ services: FULL_REPORT, source: "direct" });
    await first;

    expect(useServicesStore.getState().loading).toBe(false);
  });

  it("captures errors thrown by probeAllServices", async () => {
    mockedProbeAll.mockRejectedValueOnce(new Error("kaboom"));
    await useServicesStore.getState().diagnose();

    const s = useServicesStore.getState();
    expect(s.loading).toBe(false);
    expect(s.lastError).toBe("kaboom");
    // Belt-and-braces: report stays null because the catch path
    // doesn't synthesize one.
    expect(s.report).toBeNull();
  });
});

describe("servicesStore.probe (per-row retry)", () => {
  it("merges the single-row result into the existing report", async () => {
    // Seed an existing report.
    useServicesStore.setState({
      report: {
        services: FULL_REPORT as never,
        source: "direct",
        started_at: 1,
        finished_at: 1,
        gateway_reachable: true,
      },
    });
    mockedProbeOne.mockResolvedValueOnce(
      mkRow({
        service_type: "mqtt",
        online: false,
        version: "unknown",
        last_error: "broker unreachable",
      }),
    );
    await useServicesStore.getState().probe("mqtt");

    const s = useServicesStore.getState();
    expect(s.report?.services.mqtt.online).toBe(false);
    expect(s.report?.services.mqtt.last_error).toMatch(/broker unreachable/);
    // Other rows untouched.
    expect(s.report?.services.gateway.online).toBe(true);
    expect(s.probing.mqtt).toBe(false);
  });

  it("does not synthesize a report when none exists yet", async () => {
    // No prior diagnose() call — report is null.
    mockedProbeOne.mockResolvedValueOnce(mkRow({ service_type: "gateway" }));
    await useServicesStore.getState().probe("gateway");
    expect(useServicesStore.getState().report).toBeNull();
    expect(useServicesStore.getState().probing.gateway).toBe(false);
  });

  it("ignores a concurrent retry on the same service", async () => {
    let resolveProbe!: (v: ServiceHealth) => void;
    mockedProbeOne.mockReturnValueOnce(
      new Promise((resolve) => {
        resolveProbe = resolve as never;
      }),
    );
    const first = useServicesStore.getState().probe("gateway");
    expect(useServicesStore.getState().probing.gateway).toBe(true);

    // Concurrent retry on the same service — must be a no-op.
    await useServicesStore.getState().probe("gateway");
    expect(mockedProbeOne).toHaveBeenCalledTimes(1);

    resolveProbe(mkRow({ service_type: "gateway" }));
    await first;
    expect(useServicesStore.getState().probing.gateway).toBe(false);
  });
});

describe("servicesStore.reset", () => {
  it("clears report, loading, and lastError", async () => {
    mockedProbeAll.mockResolvedValueOnce({
      services: FULL_REPORT,
      source: "direct",
    } as never);
    await useServicesStore.getState().diagnose();
    expect(useServicesStore.getState().report).not.toBeNull();

    useServicesStore.getState().reset();
    const s = useServicesStore.getState();
    expect(s.report).toBeNull();
    expect(s.loading).toBe(false);
    expect(s.lastError).toBeNull();
    expect(s.lastProbeAt).toBeNull();
    expect(s.probing).toEqual({});
  });

  it("drops a diagnose() result that races ahead of the reset (L-2)", async () => {
    // First pass hangs; the Gateway URL changes mid-pass → reset().
    let resolveFirst!: (v: unknown) => void;
    mockedProbeAll.mockReturnValueOnce(
      new Promise((resolve) => {
        resolveFirst = resolve as never;
      }),
    );
    const first = useServicesStore.getState().diagnose();
    expect(useServicesStore.getState().loading).toBe(true);

    useServicesStore.getState().reset();

    // The old pass resolves AFTER the reset — its result must not land.
    resolveFirst({ services: FULL_REPORT, source: "direct" });
    await first;

    const s = useServicesStore.getState();
    expect(s.report).toBeNull();
    expect(s.loading).toBe(false);
    expect(s.lastError).toBeNull();
  });

  it("drops a per-row probe() result from the previous epoch (L-2)", async () => {
    useServicesStore.setState({
      report: {
        services: FULL_REPORT as never,
        source: "direct",
        started_at: 1,
        finished_at: 1,
        gateway_reachable: true,
      },
    });
    let resolveProbe!: (v: ServiceHealth) => void;
    mockedProbeOne.mockReturnValueOnce(
      new Promise((resolve) => {
        resolveProbe = resolve as never;
      }),
    );
    const first = useServicesStore.getState().probe("mqtt");
    useServicesStore.getState().reset();

    resolveProbe(mkRow({ service_type: "mqtt" }));
    await first;

    const s = useServicesStore.getState();
    // The fresh store stays clean: report null, no probing flag resurrected.
    expect(s.report).toBeNull();
    expect(s.probing.mqtt).toBeUndefined();
  });
});
