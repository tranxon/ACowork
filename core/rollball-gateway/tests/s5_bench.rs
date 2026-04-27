//! S5.12: Performance benchmark tests for Phase 4 S5.
//!
//! Lightweight benchmarks measuring key performance indicators.
//! Uses `std::time::Instant` for simplicity (no criterion dependency).
//!
//! Metrics:
//! - HTTP API response latency (P50/P99)
//! - Permission check latency
//! - GQL query latency (GrafeoStore)
//! - GrafeoStore concurrent throughput

use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use rollball_gateway::http::routes::{AppState, BridgeEvent, SharedSessionMgr, build_router};
use rollball_gateway::http::auth::HttpAuth;
use rollball_gateway::gateway::state::GatewayState;
use rollball_gateway::ipc::session::SessionManager;

// ── Helpers ───────────────────────────────────────────────────────────

fn create_test_app() -> axum::Router {
    let dir = std::env::temp_dir().join(format!(
        "rollball-s5-bench-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut gw_state = GatewayState::new(&dir.to_string_lossy());
    gw_state.config = Some(rollball_gateway::config::GatewayConfig::default());

    let session_mgr: SharedSessionMgr = std::sync::Arc::new(
        tokio::sync::Mutex::new(SessionManager::new())
    );
    let (bridge_tx, _) = tokio::sync::broadcast::channel::<BridgeEvent>(256);

    let state = AppState {
        gateway_state: std::sync::Arc::new(tokio::sync::RwLock::new(gw_state)),
        auth: std::sync::Arc::new(HttpAuth::new(false)),
        session_mgr: Some(session_mgr),
        bridge_tx: Some(bridge_tx),
    };
    build_router(state)
}

/// Calculate P50 and P99 from a sorted list of nanosecond durations.
fn percentile(mut sorted: Vec<u64>, p: f64) -> u64 {
    sorted.sort();
    let idx = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[idx.min(sorted.len()) - 1]
}

// ============================================================================
// Benchmark 1: HTTP API P99 latency
// ============================================================================

#[tokio::test]
async fn bench_s5_http_api_latency() {
    let app = create_test_app();
    let iterations = 200;
    let mut latencies_ns: Vec<u64> = Vec::with_capacity(iterations);

    // Warm up
    for _ in 0..10 {
        let _ = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
    }

    // Measure health endpoint
    for _ in 0..iterations {
        let start = Instant::now();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let elapsed = start.elapsed().as_nanos() as u64;
        assert_eq!(response.status(), StatusCode::OK);
        latencies_ns.push(elapsed);
    }

    let p50 = percentile(latencies_ns.clone(), 50.0);
    let p99 = percentile(latencies_ns.clone(), 99.0);

    println!("HTTP API /health latency: P50={:.2}ms, P99={:.2}ms",
        p50 as f64 / 1_000_000.0,
        p99 as f64 / 1_000_000.0,
    );

    // Sanity check: P99 should be under 100ms for in-process requests
    assert!(
        p99 < 100_000_000, // 100ms
        "HTTP API P99 latency too high: {:.2}ms",
        p99 as f64 / 1_000_000.0,
    );
}

#[tokio::test]
async fn bench_s5_http_config_api_latency() {
    let app = create_test_app();
    let iterations = 200;
    let mut latencies_ns: Vec<u64> = Vec::with_capacity(iterations);

    for _ in 0..iterations {
        let start = Instant::now();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/config")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let elapsed = start.elapsed().as_nanos() as u64;
        assert_eq!(response.status(), StatusCode::OK);
        latencies_ns.push(elapsed);
    }

    let p50 = percentile(latencies_ns.clone(), 50.0);
    let p99 = percentile(latencies_ns.clone(), 99.0);

    println!("HTTP API /api/config latency: P50={:.2}ms, P99={:.2}ms",
        p50 as f64 / 1_000_000.0,
        p99 as f64 / 1_000_000.0,
    );

    assert!(
        p99 < 100_000_000,
        "Config API P99 latency too high: {:.2}ms",
        p99 as f64 / 1_000_000.0,
    );
}

// ============================================================================
// Benchmark 2: Permission check latency
// ============================================================================

#[tokio::test]
async fn bench_s5_permission_check_latency() {
    use rollball_core::permission::{Permission, PermissionGrant};
    use rollball_gateway::permission_store::PermissionStore;

    let perm_store = PermissionStore::open_in_memory().unwrap();

    // Pre-grant some permissions
    let agent_id = "com.example.bench";
    for i in 0..50 {
        let perm = Permission::Network(Some(format!("https://api{}.example.com", i)));
        let grant = PermissionGrant::new(agent_id, perm, "bench");
        perm_store.grant(&grant).unwrap();
    }

    let iterations = 500;
    let mut latencies_ns: Vec<u64> = Vec::with_capacity(iterations);

    // Measure permission check (cache hit path)
    for i in 0..iterations {
        let perm = Permission::Network(Some(format!("https://api{}.example.com", i % 50)));
        let start = Instant::now();
        let result = perm_store.has_permission(agent_id, &perm);
        let elapsed = start.elapsed().as_nanos() as u64;
        assert!(result.is_ok());
        latencies_ns.push(elapsed);
    }

    let p50 = percentile(latencies_ns.clone(), 50.0);
    let p99 = percentile(latencies_ns.clone(), 99.0);

    println!("Permission check latency: P50={:.2}µs, P99={:.2}µs",
        p50 as f64 / 1_000.0,
        p99 as f64 / 1_000.0,
    );

    // Permission check should be under 5ms
    assert!(
        p99 < 5_000_000, // 5ms
        "Permission check P99 latency too high: {:.2}µs",
        p99 as f64 / 1_000.0,
    );
}

// ============================================================================
// Benchmark 3: GrafeoStore query latency
// ============================================================================

#[tokio::test]
async fn bench_s5_grafeo_query_latency() {
    use rollball_grafeo::{GrafeoStore, KnowledgeNode, KnowledgeSubType, NodeStatus, EMBEDDING_DIM};

    let store = GrafeoStore::new_in_memory().unwrap();

    // Seed data
    for i in 0..100 {
        let mut emb = vec![0.0f32; EMBEDDING_DIM];
        emb[i % EMBEDDING_DIM] = 1.0;
        let node = KnowledgeNode {
            id: None,
            subject: format!("subject_{i}"),
            predicate: "likes".to_string(),
            object: format!("object_{i}"),
            sub_type: KnowledgeSubType::Fact,
            confidence: 0.9,
            source_episode_id: None,
            embedding: Some(emb),
            status: NodeStatus::Active,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            metadata: Default::default(),
        };
        store.store_knowledge(&node).unwrap();
    }

    let iterations = 100;
    let mut latencies_ns: Vec<u64> = Vec::with_capacity(iterations);

    // Measure store_knowledge latency
    for i in 0..iterations {
        let mut emb = vec![0.0f32; EMBEDDING_DIM];
        emb[(i + 100) % EMBEDDING_DIM] = 1.0;
        let node = KnowledgeNode {
            id: None,
            subject: format!("bench_subject_{i}"),
            predicate: "knows".to_string(),
            object: format!("bench_object_{i}"),
            sub_type: KnowledgeSubType::Fact,
            confidence: 0.8,
            source_episode_id: None,
            embedding: Some(emb),
            status: NodeStatus::Active,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            metadata: Default::default(),
        };
        let start = Instant::now();
        let result = store.store_knowledge(&node);
        let elapsed = start.elapsed().as_nanos() as u64;
        assert!(result.is_ok());
        latencies_ns.push(elapsed);
    }

    let p50 = percentile(latencies_ns.clone(), 50.0);
    let p99 = percentile(latencies_ns.clone(), 99.0);

    println!("Grafeo store_knowledge latency: P50={:.2}µs, P99={:.2}µs",
        p50 as f64 / 1_000.0,
        p99 as f64 / 1_000.0,
    );
}

// ============================================================================
// Benchmark 4: GrafeoStore concurrent throughput
// ============================================================================

#[tokio::test]
async fn bench_s5_grafeo_concurrent_throughput() {
    use rollball_grafeo::{GrafeoStore, KnowledgeNode, KnowledgeSubType, NodeStatus, EMBEDDING_DIM};

    let store = Arc::new(GrafeoStore::new_in_memory().unwrap());

    // Seed data
    for i in 0..50 {
        let mut emb = vec![0.0f32; EMBEDDING_DIM];
        emb[i % EMBEDDING_DIM] = 1.0;
        let node = KnowledgeNode {
            id: None,
            subject: format!("seed_{i}"),
            predicate: "likes".to_string(),
            object: format!("seed_obj_{i}"),
            sub_type: KnowledgeSubType::Fact,
            confidence: 0.9,
            source_episode_id: None,
            embedding: Some(emb),
            status: NodeStatus::Active,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            metadata: Default::default(),
        };
        store.store_knowledge(&node).unwrap();
    }

    let num_tasks = 8;
    let ops_per_task = 50;
    let total_ops = num_tasks * ops_per_task;

    let start = Instant::now();

    let mut handles = Vec::new();
    for t in 0..num_tasks {
        let s = Arc::clone(&store);
        handles.push(tokio::spawn(async move {
            for i in 0..ops_per_task {
                let mut emb = vec![0.0f32; EMBEDDING_DIM];
                emb[(t * ops_per_task + i) % EMBEDDING_DIM] = 1.0;
                let node = KnowledgeNode {
                    id: None,
                    subject: format!("concurrent_{t}_{i}"),
                    predicate: "likes".to_string(),
                    object: format!("concurrent_obj_{t}_{i}"),
                    sub_type: KnowledgeSubType::Fact,
                    confidence: 0.8,
                    source_episode_id: None,
                    embedding: Some(emb),
                    status: NodeStatus::Active,
                    created_at: chrono::Utc::now(),
                    updated_at: chrono::Utc::now(),
                    metadata: Default::default(),
                };
                let _ = s.store_knowledge(&node);
            }
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    let elapsed = start.elapsed();
    let throughput = total_ops as f64 / elapsed.as_secs_f64();

    println!(
        "GrafeoStore concurrent write throughput: {:.0} ops/s ({} ops in {:.2}ms, {} tasks)",
        throughput,
        total_ops,
        elapsed.as_millis(),
        num_tasks,
    );

    // Sanity: should achieve at least 100 ops/s with concurrent access
    assert!(
        throughput > 100.0,
        "Concurrent throughput too low: {:.0} ops/s",
        throughput,
    );
}
