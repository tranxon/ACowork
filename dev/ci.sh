#!/bin/bash
# CI script for ACowork.AI
# Usage: ./dev/ci.sh [check|clippy|test|integration|smoke|all]

set -e

MODE=${1:-all}

# All cargo commands run against the core workspace; the red-line check
# below uses paths relative to the workspace root.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR/../core"

echo "=== ACowork.AI CI ==="

run_check() {
    echo "Running cargo check..."
    cargo check --all
}

# ADR-055 §6.20 dependency red line: acowork-node MUST NOT depend on
# acowork-gateway (mirrors acowork-node/tests/dependency_redline.rs).
run_node_redline() {
    echo "Checking acowork-node dependency red line..."
    if grep -qE '^[[:space:]]*acowork-gateway[[:space:]]*=' acowork-node/Cargo.toml; then
        echo "ERROR: acowork-node depends on acowork-gateway (ADR-055 §6.20 red line violated)"
        exit 1
    fi
    echo "acowork-node dependency red line: OK"
}

# ADR-065 §7 #1 red line: ErrorKind::MqttState MUST NOT appear outside
# acowork-mqtt-session. Reintroducing this literal in any consumer
# crate (Node / Gateway / Desktop / Runtime) recreates the original
# wake-60s bug — the local adapter fails to unwrap the inner
# StateError::Io and re-classifies ECONNRESET as fatal E4 ConfigError.
run_mqtt_redline() {
    echo "Checking MQTT ErrorKind::MqttState red line (ADR-065 §7 #1)..."
    # Scan only Rust source. Skip the shared crate (single source of truth)
    # and the mqtt-session Cargo target directory if present.
    local offenders
    offenders=$(cd "$SCRIPT_DIR/.." && grep -rnE 'ErrorKind::MqttState' \
        --include='*.rs' \
        apps/ core/ \
        | grep -vE '^(apps|core)/acowork-mqtt-session/' \
        | grep -vE '/target/' \
        || true)
    if [ -n "$offenders" ]; then
        echo "ERROR: ErrorKind::MqttState literal found outside acowork-mqtt-session (ADR-065 §7 #1):"
        echo "$offenders"
        exit 1
    fi
    echo "MQTT ErrorKind red line: OK"
}

# ADR-009 §5.4 red line: Gateway = communication + resource management +
# reverse proxy. It MUST NOT turn an installed agent's `install_path` into a
# filesystem root. Since ADR-055 that path is a *node-local* path, so such
# access silently works on a single-machine setup and breaks (5xx) the moment
# Gateway and Node are on different machines — that is the V-A/V-B bug class:
# a second parser in the Gateway reading a directory that is not there.
#
# Ceiling, not allowlist. These are the surviving legitimate call sites
# (ADR-009 §2.2 install-time package management, the Gateway-owned avatar
# cache and its publish flow, reporting `{install_path}/workspace` back to the
# Runtime). Counts may only go DOWN: the moment one goes up, or a hit appears
# in a file that is not listed here, the boundary has been re-crossed.
# Lower a number in the same commit that removes a call site.
ADR009_FS_CEILING="
gateway/mod.rs:2
http/agents.rs:4
mqtt/dispatch.rs:3
"
run_gateway_fs_redline() {
    echo "Checking ADR-009 Gateway filesystem-isolation red line..."
    local root="$SCRIPT_DIR/../core/acowork-gateway/src"
    local hits
    hits=$(grep -rnE 'install_path' --include='*.rs' "$root" \
        | grep -E 'Path::new|PathBuf::from|\.join\(|fs::read|fs::write|canonicalize|\.exists\(' \
        | grep -vE 'install_path: ' \
        | grep -vE ':[0-9]+:[[:space:]]*//' \
        || true)

    local failed=0 rel count ceiling files f
    files=$(printf '%s\n' "$hits" | grep -oE "^$root/[^:]+" | sort -u || true)
    while IFS= read -r f; do
        [ -z "$f" ] && continue
        rel="${f#"$root"/}"
        count=$(printf '%s\n' "$hits" | grep -cF "$f:" || true)
        ceiling=$(printf '%s\n' "$ADR009_FS_CEILING" | grep -E "^$rel:" | cut -d: -f2 || true)
        if [ -z "$ceiling" ]; then
            echo "ERROR: ${rel}: ${count} install_path filesystem access(es) — not on the ADR-009 §5.4 allowlist."
            failed=1
        elif [ "$count" -gt "$ceiling" ]; then
            echo "ERROR: ${rel}: ${count} install_path filesystem access(es), ceiling is ${ceiling}."
            failed=1
        fi
    done <<< "$files"

    if [ "$failed" -ne 0 ]; then
        echo ""
        echo "Agent-private data must be proxied through the Runtime, not read by the Gateway:"
        echo "  core/acowork-gateway/src/http/proxy.rs  ->  core/acowork-runtime/src/http/"
        exit 1
    fi
    echo "Gateway filesystem red line: OK"
}

run_clippy() {
    echo "Running cargo clippy..."
    cargo clippy --all-targets -- -D warnings
    echo "Running cargo clippy for acowork-embed..."
    cargo clippy -p acowork-embed --all-targets -- -D warnings
}

run_test() {
    echo "Running cargo test..."
    cargo test --all
    echo "Running acowork-embed tests..."
    cargo test -p acowork-embed
}

run_integration() {
    echo "=== Running node control-plane e2e tests (ADR-055 Phase 2) ==="
    cargo test -p acowork-gateway --test node_control_plane_e2e -- --test-threads=1
    echo "=== Running gateway settings API tests ==="
    cargo test -p acowork-gateway --test settings_api -- --test-threads=1
    echo "=== Running node wire-protocol golden tests ==="
    cargo test -p acowork-core --test node_proto_golden
    echo "=== Running acowork-node dependency red-line test ==="
    cargo test -p acowork-node --test dependency_redline
}

# Frontend smoke suite (dev/e2e_frontend_smoke/smoke_test.py): boots a
# real Gateway + local node agent against temp homes and exercises the
# HTTP/MQTT surface the desktop app talks to (config, sessions, workspaces,
# memory, docs, settings + Phase 5a auth). Requires debug binaries.
run_smoke() {
    echo "=== Building debug binaries for smoke tests ==="
    # acowork-embed needs ONNX Runtime (dev/setup_ort.sh) which may not
    # be installed on this machine; it is spawned as a sidecar and not
    # depended on by the other crates, so rebuild everything else and
    # reuse an existing embed binary when ORT is unavailable.
    cargo build --workspace --bins --exclude acowork-embed
    if [ -x target/debug/acowork-embed ]; then
        echo "acowork-embed: reusing existing binary (ORT not configured)"
    else
        echo "acowork-embed: building with download-ort feature..."
        cargo build -p acowork-embed --features download-ort \
            || echo "WARNING: acowork-embed build failed (embedding unavailable in smoke)"
    fi
    echo "=== Running frontend smoke tests ==="
    python3 -u "$SCRIPT_DIR/e2e_frontend_smoke/smoke_test.py"
}

case "$MODE" in
    check)
        run_gateway_fs_redline
        run_check
        ;;
    clippy)
        run_clippy
        ;;
    test)
        run_test
        ;;
    integration)
        run_integration
        ;;
    smoke)
        run_smoke
        ;;
    all)
        run_node_redline
        run_mqtt_redline
        run_gateway_fs_redline
        run_check
        run_clippy
        run_test
        run_integration
        run_smoke
        ;;
    *)
        echo "Unknown mode: $MODE"
        echo "Usage: $0 [check|clippy|test|integration|smoke|all]"
        exit 1
        ;;
esac

echo "=== CI completed successfully ==="
