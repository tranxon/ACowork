//! Debug HTTP endpoints for the MQTT broker (ADR-XXX).
//!
//! These endpoints exist **only** to support manual connection-recovery
//! testing. They live behind the same HTTP localhost-only gate as the
//! rest of the Gateway HTTP API (`http://127.0.0.1:19876`); they should
//! never be exposed to the network.
//!
//! - `POST /api/debug/mqtt/shutdown` — request the broker thread to
//!   exit and close its TCP listener (Runtime + Desktop will see a
//!   clean disconnect and enter their reconnect cycles).
//! - `POST /api/debug/mqtt/start` — restart the broker thread.
//!
//! Both endpoints are idempotent: calling shutdown twice reports
//! `already_stopped`, and calling start twice reports `already_running`.
//!
//! **All retry / TIME_WAIT workarounds live here**, not in
//! `mqtt/broker.rs`. The broker itself stays a simple
//! "start-once, run-until-process-exits" component.

use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::post, Json, Router};
use serde::Serialize;
use std::time::Duration;

use crate::http::routes::AppState;

/// Response body for debug endpoints.
#[derive(Debug, Serialize)]
pub struct DebugResponse {
    pub ok: bool,
    pub message: String,
}

/// Build the debug router. Mounted at `/api/debug/mqtt/*`.
pub fn debug_mqtt_routes() -> Router<AppState> {
    Router::new()
        .route("/api/debug/mqtt/shutdown", post(shutdown_mqtt_broker))
        .route("/api/debug/mqtt/start", post(start_mqtt_broker))
}

/// How long to sleep after `signal_shutdown` so the broker thread has
/// time to unpark (it uses `park_timeout(200ms)`) and drop the
/// listener. Without this, an immediate rebind races the OS TIME_WAIT
/// state and fails with EADDRINUSE.
const SHUTDOWN_DRAIN: Duration = Duration::from_millis(400);

/// Max retries for `/start` to absorb transient EADDRINUSE caused by
/// the OS holding the port in TIME_WAIT after the previous broker
/// closed. 5 attempts × 200 ms ≈ 1 s of patience.
const START_RETRIES: u32 = 5;
const START_RETRY_DELAY: Duration = Duration::from_millis(200);

/// Request the broker thread to exit.
///
/// After sending the shutdown signal we sleep briefly to let the
/// broker thread (which uses `park_timeout(200ms)`) wake up, drop the
/// `Broker`, and close the TCP listener. This avoids an immediate
/// rebind on `/start` racing against TIME_WAIT.
async fn shutdown_mqtt_broker(State(state): State<AppState>) -> impl IntoResponse {
    let taken = {
        let gw = state.gateway_state.read().await;
        let mut ctrl = gw.mqtt_broker_control.lock().await;
        match ctrl.take() {
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(DebugResponse {
                        ok: false,
                        message: "MQTT broker is not running (or was already shut down)"
                            .to_string(),
                    }),
                )
                    .into_response();
            }
            Some(h) => h,
        }
    };

    let mut taken = taken;
    let listen_addr = taken.listen_addr;
    if let Err(e) = taken.signal_shutdown() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(DebugResponse {
                ok: false,
                message: format!("Failed to shutdown MQTT broker: {}", e),
            }),
        )
            .into_response();
    }

    // Drain TIME_WAIT before the next /start can rebind the same port.
    tokio::time::sleep(SHUTDOWN_DRAIN).await;

    tracing::info!(
        addr = %listen_addr,
        "MQTT broker shut down via debug endpoint"
    );
    (
        StatusCode::OK,
        Json(DebugResponse {
            ok: true,
            message: "MQTT broker shut down".to_string(),
        }),
    )
        .into_response()
}

/// Restart the broker thread, retrying on transient bind failures.
async fn start_mqtt_broker(State(state): State<AppState>) -> impl IntoResponse {
    // Bail early if the broker is already running.
    {
        let gw = state.gateway_state.read().await;
        let ctrl = gw.mqtt_broker_control.lock().await;
        if ctrl.is_some() {
            return (
                StatusCode::CONFLICT,
                Json(DebugResponse {
                    ok: false,
                    message: "MQTT broker is already running".to_string(),
                }),
            )
                .into_response();
        }
    }

    // Resolve MQTT host/port from the persisted Gateway config.
    let (host, port) = {
        let gw = state.gateway_state.read().await;
        let cfg = match gw.config.as_ref() {
            Some(c) => c,
            None => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(DebugResponse {
                        ok: false,
                        message: "Gateway config not loaded".to_string(),
                    }),
                )
                    .into_response();
            }
        };
        if !cfg.mqtt.enabled {
            return (
                StatusCode::CONFLICT,
                Json(DebugResponse {
                    ok: false,
                    message: "MQTT is disabled in Gateway config".to_string(),
                }),
            )
                .into_response();
        }
        (cfg.mqtt.host.clone(), cfg.mqtt.port)
    };

    // Retry the start a few times. The most common failure here is the
    // OS still holding the port in TIME_WAIT after a previous broker
    // listener closed; a short sleep + retry is enough to absorb it.
    let mut last_err: Option<String> = None;
    for attempt in 0..START_RETRIES {
        match crate::mqtt::start_broker(&host, port) {
            Ok(handle) => {
                let gw = state.gateway_state.read().await;
                {
                    let mut ctrl = gw.mqtt_broker_control.lock().await;
                    *ctrl = Some(handle);
                }
                tracing::info!(
                    host = %host,
                    port,
                    attempt,
                    "MQTT broker restarted via debug endpoint"
                );
                return (
                    StatusCode::OK,
                    Json(DebugResponse {
                        ok: true,
                        message: format!("MQTT broker restarted on {}:{}", host, port),
                    }),
                )
                    .into_response();
            }
            Err(e) => {
                let msg = e.to_string();
                tracing::warn!(
                    host = %host,
                    port,
                    attempt,
                    error = %msg,
                    "MQTT broker start attempt failed; retrying"
                );
                last_err = Some(msg);
                if attempt + 1 < START_RETRIES {
                    tokio::time::sleep(START_RETRY_DELAY).await;
                }
            }
        }
    }

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(DebugResponse {
            ok: false,
            message: format!(
                "Failed to start MQTT broker after {} attempts: {}",
                START_RETRIES,
                last_err.unwrap_or_default()
            ),
        }),
    )
        .into_response()
}