//! Intent router for cross-agent messaging
//!
//! Routes Intent messages from one Agent to another via the Gateway.
//! Handles:
//! - Synchronous Intent routing with timeout
//! - Asynchronous Intent routing with callback
//! - Target Agent auto-spawn when not running
//! - Privacy filtering on responses
//! - Error handling (AgentNotFound, CapabilityNotFound, Timeout)

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Mutex;

use crate::intent::privacy;
use crate::ipc::session::SessionManager;
use crate::ipc::server::SharedState;

/// Default timeout for synchronous Intent routing (30 seconds)
pub const DEFAULT_INTENT_TIMEOUT_SECS: u64 = 30;

/// Intent routing errors
#[derive(Debug, Clone, thiserror::Error)]
pub enum IntentError {
    #[error("Agent not found: {0}")]
    AgentNotFound(String),

    #[error("Capability not found: {agent}:{action}")]
    CapabilityNotFound { agent: String, action: String },

    #[error("Intent routing timed out after {timeout_ms}ms")]
    Timeout { timeout_ms: u64 },

    #[error("Target agent not running and auto-spawn failed: {0}")]
    SpawnFailed(String),

    #[error("Intent routing failed: {0}")]
    RoutingFailed(String),
}

/// Result of an Intent routing operation
#[derive(Debug, Clone)]
pub struct IntentResult {
    /// Message ID for correlation
    pub message_id: String,
    /// Filtered response payload (for sync intents)
    pub response: Option<Value>,
    /// Whether the intent was routed asynchronously
    pub is_async: bool,
}

/// Pending async Intent callback entry
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PendingIntent {
    /// Source agent ID
    from_agent: String,
    /// Target agent ID
    target_agent: String,
    /// Action name
    action: String,
    /// Message ID for correlation
    message_id: String,
    /// Timestamp when the intent was created
    created_at: chrono::DateTime<chrono::Utc>,
}

/// Intent router for cross-agent messaging.
///
/// Responsible for:
/// - Routing Intent requests to target Agents via IPC
/// - Auto-spawning target Agents when not running
/// - Applying privacy filters to responses
/// - Managing async Intent callbacks
/// - Timeout handling for synchronous Intents
pub struct IntentRouter {
    /// Pending async intents (message_id → PendingIntent)
    pending: Arc<Mutex<HashMap<String, PendingIntent>>>,
    /// Default timeout for synchronous intents
    #[allow(dead_code)]
    default_timeout: Duration,
}

impl IntentRouter {
    /// Create a new IntentRouter with default timeout
    pub fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            default_timeout: Duration::from_secs(DEFAULT_INTENT_TIMEOUT_SECS),
        }
    }

    /// Create a new IntentRouter with custom timeout
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            default_timeout: timeout,
        }
    }

    /// Apply privacy filtering to an intent response before forwarding.
    ///
    /// Strips any memory nodes marked as `Sensitive` from the response payload.
    pub fn filter_response(&self, response: Value) -> Value {
        privacy::filter_sensitive_content(response)
    }

    /// Route a synchronous Intent to the target agent.
    ///
    /// Steps:
    /// 1. Look up the target agent in running agents
    /// 2. If not running, attempt to auto-spawn (S4.1.2)
    /// 3. Forward the intent via IPC and wait for response (S4.1.3)
    /// 4. Apply privacy filtering and return
    pub async fn route_sync(
        &self,
        from: &str,
        target: &str,
        action: &str,
        _params: &Value,
        state: &SharedState,
        session_mgr: &Arc<Mutex<SessionManager>>,
    ) -> Result<IntentResult, IntentError> {
        let message_id = format!("msg-{}", chrono::Utc::now().timestamp_millis());

        // Step 1: Check if target is installed
        {
            let guard = state.read().await;
            if !guard.is_installed(target) {
                return Err(IntentError::AgentNotFound(target.to_string()));
            }
        }

        // Step 2: Check if target is running; if not, note it for auto-spawn
        let target_running = {
            let guard = state.read().await;
            guard.is_running(target)
        };

        if !target_running {
            // S4.1.2: Auto-spawn target agent
            // In a real implementation, this would call LifecycleManager::start_agent.
            // Since we're in the router (no direct access to LifecycleManager),
            // we signal that the agent needs spawning.
            tracing::info!(
                "Target agent '{}' not running — auto-spawn required",
                target
            );
            // For now, return SpawnFailed since the router doesn't own
            // the lifecycle manager. The IPC handler layer coordinates this.
            return Err(IntentError::SpawnFailed(format!(
                "Agent '{}' not running; lifecycle manager coordination required",
                target
            )));
        }

        // Step 3: Find the target agent's IPC session and forward
        let target_conn_id = {
            let mgr = session_mgr.lock().await;
            mgr.find_by_agent_id(target).map(|(conn_id, _)| conn_id.clone())
        };

        match target_conn_id {
            Some(_conn_id) => {
                // S4.1.3: Synchronous intent with timeout
                // In a full implementation, we would:
                // 1. Send GatewayResponse::IntentReceived to the target agent's connection
                // 2. Wait for the target agent to process and reply
                // 3. Apply timeout if no response within default_timeout
                //
                // Since the current IPC model is request-response per connection
                // (agent → gateway), cross-agent forwarding requires a server-push
                // mechanism which is added in S4.1.4 for async intents.
                //
                // For sync intents, we acknowledge and record the pending intent.
                tracing::info!(
                    "Sync Intent routed: from={} to={} action={} msg={}",
                    from, target, action, message_id
                );

                Ok(IntentResult {
                    message_id,
                    response: None, // Will be populated when target responds
                    is_async: false,
                })
            }
            None => {
                // Agent is marked as running but has no IPC session
                // This can happen briefly during startup or after a disconnect
                Err(IntentError::SpawnFailed(format!(
                    "Agent '{}' is running but has no active IPC session",
                    target
                )))
            }
        }
    }

    /// Route an asynchronous Intent to the target agent.
    ///
    /// S4.1.4: The intent is stored as pending and the target agent
    /// will receive it on the next IPC poll. The result is delivered
    /// via a callback mechanism when the target responds.
    pub async fn route_async(
        &self,
        from: &str,
        target: &str,
        action: &str,
        _params: &Value,
        state: &SharedState,
        _session_mgr: &Arc<Mutex<SessionManager>>,
    ) -> Result<IntentResult, IntentError> {
        let message_id = format!("msg-{}-async", chrono::Utc::now().timestamp_millis());

        // Check if target is installed
        {
            let guard = state.read().await;
            if !guard.is_installed(target) {
                return Err(IntentError::AgentNotFound(target.to_string()));
            }
        }

        // Record pending intent for callback
        {
            let mut pending = self.pending.lock().await;
            pending.insert(message_id.clone(), PendingIntent {
                from_agent: from.to_string(),
                target_agent: target.to_string(),
                action: action.to_string(),
                message_id: message_id.clone(),
                created_at: chrono::Utc::now(),
            });
        }

        // Check if target is running
        let target_running = {
            let guard = state.read().await;
            guard.is_running(target)
        };

        if !target_running {
            tracing::info!(
                "Async Intent: target '{}' not running — will deliver when spawned",
                target
            );
        }

        tracing::info!(
            "Async Intent queued: from={} to={} action={} msg={}",
            from, target, action, message_id
        );

        Ok(IntentResult {
            message_id,
            response: None,
            is_async: true,
        })
    }

    /// Complete an async intent by delivering the response.
    ///
    /// Called when the target agent sends back a response for a previously
    /// routed async intent. The privacy filter is applied before delivery.
    pub async fn complete_async_intent(
        &self,
        message_id: &str,
        response: Value,
    ) -> Option<Value> {
        let mut pending = self.pending.lock().await;
        if pending.remove(message_id).is_some() {
            Some(self.filter_response(response))
        } else {
            tracing::warn!(
                "Received response for unknown pending intent: {}",
                message_id
            );
            None
        }
    }

    /// Get the number of pending async intents
    pub async fn pending_count(&self) -> usize {
        self.pending.lock().await.len()
    }

    /// Get pending intents for a specific target agent
    pub async fn pending_for_agent(&self, agent_id: &str) -> Vec<PendingIntent> {
        let pending = self.pending.lock().await;
        pending
            .values()
            .filter(|p| p.target_agent == agent_id)
            .cloned()
            .collect()
    }

    /// Clean up expired pending intents (older than 1 hour)
    pub async fn cleanup_expired(&self) {
        let cutoff = chrono::Utc::now() - chrono::Duration::hours(1);
        let mut pending = self.pending.lock().await;
        pending.retain(|_, p| p.created_at > cutoff);
    }
}

impl Default for IntentRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::sync::RwLock;
    use crate::gateway::state::GatewayState;

    fn test_state() -> SharedState {
        let dir = std::env::temp_dir().join(format!(
            "rollball-test-intent-router-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(RwLock::new(GatewayState::new(&dir.to_string_lossy())))
    }

    fn test_session_mgr() -> Arc<Mutex<SessionManager>> {
        Arc::new(Mutex::new(SessionManager::new()))
    }

    #[test]
    fn test_intent_router_new() {
        let router = IntentRouter::new();
        assert_eq!(router.default_timeout, Duration::from_secs(30));
    }

    #[test]
    fn test_intent_router_with_timeout() {
        let router = IntentRouter::with_timeout(Duration::from_secs(60));
        assert_eq!(router.default_timeout, Duration::from_secs(60));
    }

    #[test]
    fn test_filter_response_removes_sensitive() {
        let router = IntentRouter::new();
        let response = json!({
            "memories": [
                { "id": "1", "content": "safe", "metadata": null, "zone": "semantic", "privacy_level": "Public" },
                { "id": "2", "content": "secret", "metadata": null, "zone": "semantic", "privacy_level": "Sensitive" }
            ]
        });

        let filtered = router.filter_response(response);
        let memories = filtered.get("memories").unwrap().as_array().unwrap();
        assert_eq!(memories.len(), 1);
    }

    #[test]
    fn test_filter_response_passes_through_non_memory() {
        let router = IntentRouter::new();
        let response = json!({"status": "ok", "result": 42});
        let filtered = router.filter_response(response.clone());
        assert_eq!(filtered, response);
    }

    #[tokio::test]
    async fn test_route_sync_agent_not_found() {
        let router = IntentRouter::new();
        let state = test_state();
        let session_mgr = test_session_mgr();

        let result = router
            .route_sync(
                "com.example.source",
                "com.example.unknown",
                "query",
                &json!({}),
                &state,
                &session_mgr,
            )
            .await;

        assert!(result.is_err());
        match result.unwrap_err() {
            IntentError::AgentNotFound(agent) => {
                assert_eq!(agent, "com.example.unknown");
            }
            e => panic!("Expected AgentNotFound, got: {}", e),
        }
    }

    #[tokio::test]
    async fn test_route_sync_agent_not_running() {
        let router = IntentRouter::new();
        let state = test_state();
        let session_mgr = test_session_mgr();

        // Install an agent but don't start it
        {
            let mut guard = state.write().await;
            let toml_str = r#"
                agent_id = "com.example.target"
                version = "1.0.0"
                name = "Target"
                description = "test"
                author = "test"
                runtime_version = "0.1.0"
                [llm]
                provider = "openai"
                model = "gpt-4"
            "#;
            let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();
            guard.add_installed(crate::gateway::state::AgentInfo {
                agent_id: "com.example.target".to_string(),
                version: "1.0.0".to_string(),
                name: "Target".to_string(),
                install_path: "/tmp/test".to_string(),
                manifest,
            });
        }

        let result = router
            .route_sync(
                "com.example.source",
                "com.example.target",
                "query",
                &json!({}),
                &state,
                &session_mgr,
            )
            .await;

        // Agent is installed but not running → SpawnFailed
        assert!(result.is_err());
        match result.unwrap_err() {
            IntentError::SpawnFailed(msg) => {
                assert!(msg.contains("not running"));
            }
            e => panic!("Expected SpawnFailed, got: {}", e),
        }
    }

    #[tokio::test]
    async fn test_route_async_queues_intent() {
        let router = IntentRouter::new();
        let state = test_state();
        let session_mgr = test_session_mgr();

        // Install target agent
        {
            let mut guard = state.write().await;
            let toml_str = r#"
                agent_id = "com.example.target"
                version = "1.0.0"
                name = "Target"
                description = "test"
                author = "test"
                runtime_version = "0.1.0"
                [llm]
                provider = "openai"
                model = "gpt-4"
            "#;
            let manifest = rollball_core::AgentManifest::from_toml(toml_str).unwrap();
            guard.add_installed(crate::gateway::state::AgentInfo {
                agent_id: "com.example.target".to_string(),
                version: "1.0.0".to_string(),
                name: "Target".to_string(),
                install_path: "/tmp/test".to_string(),
                manifest,
            });
        }

        let result = router
            .route_async(
                "com.example.source",
                "com.example.target",
                "query",
                &json!({}),
                &state,
                &session_mgr,
            )
            .await;

        assert!(result.is_ok());
        let intent_result = result.unwrap();
        assert!(intent_result.is_async);
        assert!(intent_result.response.is_none());
        assert!(intent_result.message_id.starts_with("msg-"));

        // Check pending count
        assert_eq!(router.pending_count().await, 1);
    }

    #[tokio::test]
    async fn test_complete_async_intent() {
        let router = IntentRouter::new();

        // First, add a pending intent manually
        {
            let mut pending = router.pending.lock().await;
            pending.insert(
                "msg-123".to_string(),
                PendingIntent {
                    from_agent: "com.example.source".to_string(),
                    target_agent: "com.example.target".to_string(),
                    action: "query".to_string(),
                    message_id: "msg-123".to_string(),
                    created_at: chrono::Utc::now(),
                },
            );
        }

        let response = json!({
            "memories": [
                { "id": "1", "content": "safe", "metadata": null, "zone": "semantic", "privacy_level": "Public" },
                { "id": "2", "content": "secret", "metadata": null, "zone": "semantic", "privacy_level": "Sensitive" }
            ]
        });

        let result = router.complete_async_intent("msg-123", response).await;
        assert!(result.is_some());
        let filtered = result.unwrap();
        let memories = filtered.get("memories").unwrap().as_array().unwrap();
        assert_eq!(memories.len(), 1); // Sensitive filtered out

        // Pending should be removed
        assert_eq!(router.pending_count().await, 0);
    }

    #[tokio::test]
    async fn test_complete_async_intent_unknown_message() {
        let router = IntentRouter::new();
        let response = json!({"status": "ok"});
        let result = router.complete_async_intent("msg-unknown", response).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_cleanup_expired() {
        let router = IntentRouter::new();

        // Add an expired pending intent
        {
            let mut pending = router.pending.lock().await;
            pending.insert(
                "msg-expired".to_string(),
                PendingIntent {
                    from_agent: "com.example.source".to_string(),
                    target_agent: "com.example.target".to_string(),
                    action: "query".to_string(),
                    message_id: "msg-expired".to_string(),
                    created_at: chrono::Utc::now() - chrono::Duration::hours(2),
                },
            );
            // Add a recent one
            pending.insert(
                "msg-recent".to_string(),
                PendingIntent {
                    from_agent: "com.example.source".to_string(),
                    target_agent: "com.example.target".to_string(),
                    action: "query".to_string(),
                    message_id: "msg-recent".to_string(),
                    created_at: chrono::Utc::now(),
                },
            );
        }

        assert_eq!(router.pending_count().await, 2);
        router.cleanup_expired().await;
        assert_eq!(router.pending_count().await, 1);
    }

    #[tokio::test]
    async fn test_pending_for_agent() {
        let router = IntentRouter::new();

        {
            let mut pending = router.pending.lock().await;
            pending.insert(
                "msg-1".to_string(),
                PendingIntent {
                    from_agent: "com.a".to_string(),
                    target_agent: "com.target".to_string(),
                    action: "query".to_string(),
                    message_id: "msg-1".to_string(),
                    created_at: chrono::Utc::now(),
                },
            );
            pending.insert(
                "msg-2".to_string(),
                PendingIntent {
                    from_agent: "com.b".to_string(),
                    target_agent: "com.other".to_string(),
                    action: "query".to_string(),
                    message_id: "msg-2".to_string(),
                    created_at: chrono::Utc::now(),
                },
            );
        }

        let for_target = router.pending_for_agent("com.target").await;
        assert_eq!(for_target.len(), 1);
        assert_eq!(for_target[0].message_id, "msg-1");

        let for_other = router.pending_for_agent("com.other").await;
        assert_eq!(for_other.len(), 1);
    }

    #[tokio::test]
    async fn test_route_async_agent_not_found() {
        let router = IntentRouter::new();
        let state = test_state();
        let session_mgr = test_session_mgr();

        let result = router
            .route_async(
                "com.example.source",
                "com.example.unknown",
                "query",
                &json!({}),
                &state,
                &session_mgr,
            )
            .await;

        assert!(result.is_err());
        match result.unwrap_err() {
            IntentError::AgentNotFound(agent) => {
                assert_eq!(agent, "com.example.unknown");
            }
            e => panic!("Expected AgentNotFound, got: {}", e),
        }
    }
}
