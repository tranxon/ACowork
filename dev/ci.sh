#!/bin/bash
# CI script for ACowork.AI
# Usage: ./dev/ci.sh [check|clippy|test|integration|all]

set -e

MODE=${1:-all}

echo "=== ACowork.AI CI ==="

run_check() {
    echo "Running cargo check..."
    cargo check --all
}

# ADR-055 §6.20 dependency red line: acowork-node MUST NOT depend on
# acowork-gateway (mirrors acowork-node/tests/dependency_redline.rs).
run_node_redline() {
    echo "Checking acowork-node dependency red line..."
    if grep -qE '^[[:space:]]*acowork-gateway[[:space:]]*=' core/acowork-node/Cargo.toml; then
        echo "ERROR: acowork-node depends on acowork-gateway (ADR-055 §6.20 red line violated)"
        exit 1
    fi
    echo "acowork-node dependency red line: OK"
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
    echo "=== Running tool call e2e tests ==="
    cargo test --test tool_call_e2e -- --test-threads=1
    echo "=== Running tool call stress tests ==="
    cargo test --test tool_call_stress -- --test-threads=1
    echo "=== Running history recovery e2e tests ==="
    cargo test --test history_recovery_e2e -- --test-threads=1

    # Optional: Real LLM integration tests (requires MINIMAX_API_KEY)
    if [ -n "$MINIMAX_API_KEY" ]; then
        echo "=== Running real LLM integration tests ==="
        cargo test --test llm_integration -- --ignored --test-threads=1
        cargo test --test history_recovery_e2e -- --ignored --test-threads=1
    fi
}

case "$MODE" in
    check)
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
    all)
        run_node_redline
        run_check
        run_clippy
        run_test
        run_integration
        ;;
    *)
        echo "Unknown mode: $MODE"
        echo "Usage: $0 [check|clippy|test|integration|all]"
        exit 1
        ;;
esac

echo "=== CI completed successfully ==="
