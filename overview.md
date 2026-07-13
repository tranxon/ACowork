# ADR-033 MQTT Migration — Full Implementation Overview

## Summary

Phase 1-3 of the ADR-033 MQTT protocol migration are now implemented. The
MQTT-based event bus replaces the previous gRPC + WebSocket protocol stack
across Gateway, Runtime, and Desktop:

| Phase | Scope | Status |
|-------|-------|--------|
| **P1** | Gateway MQTT broker + publisher | ✅ Complete (24 tests) |
| **P2** | Runtime MQTT client + HTTP server | ✅ Complete (8 tests) |
| **P3** | Desktop Tauri MQTT commands | ✅ Complete (building) |
| **P4** | End-to-end verification + gRPC cleanup | 📋 Planned |

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│ Gateway Process                                              │
│ ├── HTTP Server (Axum, port 19876)                           │
│ ├── gRPC Server (Tonic, port 19877) ← Phase 4 will delete   │
│ ├── MQTT Broker (rumqttd, port 19875)                        │
│ ├── MQTT Publisher (gateway:publisher)                       │
│ │   └── acowork/global/{kind} Retained QoS 1                 │
│ ├── RuntimeHttpRegistry (reverse proxy)                      │
│ └── /api/global/{kind} HTTP facade                           │
└─────────────────────────────────────────────────────────────┘
              │ MQTT                            │ HTTP
              ▼                                 ▼
┌─────────────────────────┐    ┌────────────────────────────────┐
│ Runtime Process          │    │ Desktop Tauri Process           │
│ ├── gRPC Client          │    │ ├── Gateway HTTP Client         │
│ ├── MQTT Client (NEW)    │    │ ├── MQTT Client (NEW)           │
│ │   ├── agent:{id}       │    │ │   ├── SUB agents/+/status     │
│ │   ├── LWT: offline     │    │ │   ├── SUB agents/+/sessions/* │
│ │   ├── PUB status/meta  │    │ │   └── → emit("mqtt-event")   │
│ │   ├── SUB global/#     │    │ └── Commands:                   │
│ │   └── SUB control/#    │    │     ├── connect_mqtt            │
│ ├── HTTP Server (NEW)    │    │     ├── mqtt_publish_control    │
│ │   └── GET /sessions    │    │     └── disconnect_mqtt         │
│ └── AvailableCache (NEW) │    └────────────────────────────────┘
└─────────────────────────┘
```

## New files (6 Phase 1 + 6 Phase 2 + 2 Phase 3 = 14)

### Phase 1 — Gateway Infrastructure
| File | Purpose |
|------|---------|
| `core/acowork-core/proto/mqtt_payload.proto` | Independent protobuf (all message types) |
| `core/acowork-gateway/src/mqtt/broker.rs` | Embedded rumqttd broker |
| `core/acowork-gateway/src/mqtt/client.rs` | Gateway MQTT client |
| `core/acowork-gateway/src/mqtt/global_resources_publisher.rs` | Publish acowork/global/{kind} |
| `core/acowork-gateway/src/mqtt/acl.rs` | ACL config |
| `core/acowork-gateway/src/http/global.rs` | /api/global/{kind} facade |

### Phase 2 — Runtime MQTT
| File | Purpose |
|------|---------|
| `core/acowork-runtime/src/mqtt/client.rs` | Runtime MQTT client (LWT + publish status/meta/config) |
| `core/acowork-runtime/src/mqtt/available_cache.rs` | Cache acowork/global/# available state |
| `core/acowork-runtime/src/http/server.rs` | Runtime localhost HTTP server (reverse proxy backend) |
| `core/acowork-gateway/src/http/proxy.rs` | Gateway reverse proxy + RuntimeHttpRegistry |
| `core/acowork-runtime/src/mqtt/mod.rs` | Runtime MQTT module entry |
| `core/acowork-runtime/src/http/mod.rs` | Runtime HTTP module entry |

### Phase 3 — Desktop Tauri MQTT
| File | Purpose |
|------|---------|
| `apps/acowork-desktop/src-tauri/src/mqtt_client.rs` | Desktop MQTT client (subscribe + publish control) |
| `apps/acowork-desktop/src-tauri/src/commands/chat_mqtt.rs` | Tauri commands (connect/disconnect/control) |

## Modified files (18 total)

**Phase 1 (10):** `build.rs`, `lib.rs`(core), `defaults.rs`, workspace `Cargo.toml`, Gateway `Cargo.toml`, Gateway `lib.rs`, `config.rs`, `gateway/mod.rs`, `http/mod.rs`, `http/routes.rs`

**Phase 2 (10):** Runtime `Cargo.toml`, Runtime `lib.rs`, `cli.rs`, `config.rs`, `startup/context.rs`, `startup/agent_init.rs`, Gateway `http/mod.rs`, `http/routes.rs`, `http/agents.rs`, `http/server.rs`

**Phase 3 (4):** Desktop `Cargo.toml`, `state.rs`, `commands/mod.rs`, `lib.rs`

## Key design decisions

1. **rumqttd 0.20 + rumqttc 0.25** — latest stable (ADR mentioned 0.14/0.24)
2. **Independent proto namespace** — `acowork.mqtt.v1`, no shared defs
3. **Dual-channel coexistence** — gRPC + MQTT run in parallel until Phase 4 cleanup
4. **Last Will** — no "ghost online" on Runtime crash
5. **Retained messages** — subscribers get latest state immediately on connect
6. **Reverse proxy** — Gateway forwards large data queries to Runtime localhost HTTP
7. **Fire-and-forget MQTT** — control commands without ack go via MQTT; with ack via HTTP
8. **Tauri events** — MQTT messages emitted to React frontend via `app.emit("mqtt-event")`
