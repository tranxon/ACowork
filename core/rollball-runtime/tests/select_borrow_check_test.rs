//! Verification: tokio::select! borrow checker compatibility with Scheme B
//!
//! This test proves (or disproves) that the proposed `tokio::select!` pattern
//! in `run_gateway_loop()` compiles correctly with the real type signatures.
//!
//! Key signatures to verify:
//!   - AgentLoop::run(&mut self, user_message: &str, context_builder: &ContextBuilder) -> Result<String>
//!   - GatewayGrpcClient::recv_message(&mut self) -> Result<Option<GatewayResponse>>
//!
//! Questions:
//!   Q1: Can select! poll agent_loop.run() and grpc_client.recv_message() simultaneously?
//!   Q2: Can we access &mut agent_loop in the recv_message branch?
//!   Q3: Can we access &mut context_builder in the recv_message branch?
//!   Q4: Can we access &mut grpc_client in the recv_message branch (for handlers)?

use rollball_runtime::agent::context::ContextBuilder;
use rollball_runtime::agent::inbound::InboundMessage;

/// Q1 + Q4: Basic select! pattern — two different &mut objects in select!
///
/// This tests the core claim: `agent_loop.run()` borrows `&mut agent_loop`,
/// and `grpc_client.recv_message()` borrows `&mut grpc_client`. Since these
/// are DIFFERENT objects, the borrow checker should allow both.
///
/// After recv_message() returns, `&mut grpc_client` is released, so handlers
/// that need `&mut grpc_client` (like send_intent, send_session_response)
/// should also work.
#[tokio::test]
async fn test_select_basic_two_mut_objects() {
    // This test is a structural compile-time verification.
    // We can't actually construct these types without a running Gateway,
    // so we use a simplified mock that mirrors the exact signatures.

    // The real signatures:
    //   AgentLoop::run(&mut self, &str, &ContextBuilder) -> Result<String>
    //   GatewayGrpcClient::recv_message(&mut self) -> Result<Option<GatewayResponse>>

    // Simplified mock to verify the borrow pattern:
    struct MockAgentLoop;
    impl MockAgentLoop {
        async fn run(&mut self, _msg: &str, _ctx: &ContextBuilder) -> Result<String, ()> {
            // Simulate long-running work with yields
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            Ok("response".to_string())
        }
    }

    struct MockGrpcClient {
        rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    }
    impl MockGrpcClient {
        async fn recv_message(&mut self) -> Option<String> {
            self.rx.recv().await
        }

        async fn send_response(&mut self, _msg: &str) {
            // Simulate sending
        }
    }

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut agent_loop = MockAgentLoop;
    let mut grpc_client = MockGrpcClient { rx };
    let inbound_tx: tokio::sync::mpsc::Sender<InboundMessage> =
        tokio::sync::mpsc::channel(16).0;

    // Send a message so recv_message() returns something
    tx.send("test_message".to_string()).unwrap();

    // This is the Scheme B pattern
    let content = "hello";
    let context_builder = ContextBuilder::new("system prompt".to_string());

    let agent_fut = agent_loop.run(content, &context_builder);
    tokio::pin!(agent_fut);

    let agent_result = loop {
        tokio::select! {
            result = &mut agent_fut => {
                // agent_loop.run() completed
                break result;
            }
            msg = grpc_client.recv_message() => {
                // grpc_client.recv_message() returned
                // After this, &mut grpc_client is released, so we can use it again
                if let Some(message) = msg {
                    match message.as_str() {
                        "continue_execution" => {
                            let _ = inbound_tx.send(InboundMessage::ContinueExecution {
                                reason: "user_requested".to_string(),
                            }).await;
                        }
                        "interrupt" => {
                            let _ = inbound_tx.send(InboundMessage::Interrupt {
                                reason: "user_requested".to_string(),
                            }).await;
                        }
                        "session_query" => {
                            // ✅ Can use &mut grpc_client here — recv_message() has returned
                            grpc_client.send_response("session_data").await;
                        }
                        _ => {}
                    }
                }
            }
        }
    };

    // Verify the agent result
    assert!(agent_result.is_ok());
    assert_eq!(agent_result.unwrap(), "response");
}

/// Q2: Can we access &mut agent_loop in the recv_message branch?
///
/// This should FAIL to compile because `agent_fut` holds `&mut agent_loop`,
/// and we can't have another `&mut agent_loop` while the future is alive.
#[tokio::test]
async fn test_select_cannot_mutate_agent_loop_in_recv_branch() {
    #[allow(dead_code)]
    struct MockAgentLoop {
        #[allow(dead_code)]
        value: i32,
    }
    impl MockAgentLoop {
        async fn run(&mut self, _msg: &str) -> Result<String, ()> {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            Ok("response".to_string())
        }
        #[allow(dead_code)]
        fn update_value(&mut self, new_val: i32) {
            self.value = new_val;
        }
    }

    struct MockGrpcClient {
        rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    }
    impl MockGrpcClient {
        async fn recv_message(&mut self) -> Option<String> {
            self.rx.recv().await
        }
    }

    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut agent_loop = MockAgentLoop { value: 0 };
    let mut grpc_client = MockGrpcClient { rx };

    let agent_fut = agent_loop.run("hello");
    tokio::pin!(agent_fut);

    let _result = loop {
        tokio::select! {
            result = &mut agent_fut => {
                break result;
            }
            msg = grpc_client.recv_message() => {
                if let Some(_message) = msg {
                    // ❌ This should NOT compile: agent_loop is already mutably
                    //    borrowed by agent_fut.
                    // agent_loop.update_value(42);
                    //
                    // Expected compile error:
                    //   error[E0499]: cannot borrow `agent_loop` as mutable more than once
                    //   --> borrow of `agent_loop` occurs here
                    //   --> first mutable borrow occurs due to call to `run` on line X
                }
            }
        }
    };
}

/// Q3: Can we access &mut context_builder in the recv_message branch?
///
/// `agent_loop.run()` takes `&ContextBuilder` (immutable borrow).
/// We need `&mut ContextBuilder` for `set_override_model()` etc.
/// This should FAIL to compile because `agent_fut` holds `&context_builder`,
/// and we can't have `&mut context_builder` while a `&context_builder` exists.
#[tokio::test]
async fn test_select_cannot_mutate_context_builder_in_recv_branch() {
    struct MockAgentLoop;
    impl MockAgentLoop {
        async fn run(&mut self, _msg: &str, _ctx: &ContextBuilder) -> Result<String, ()> {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            Ok("response".to_string())
        }
    }

    struct MockGrpcClient {
        rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    }
    impl MockGrpcClient {
        async fn recv_message(&mut self) -> Option<String> {
            self.rx.recv().await
        }
    }

    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut agent_loop = MockAgentLoop;
    let mut grpc_client = MockGrpcClient { rx };
    let context_builder = ContextBuilder::new("system prompt".to_string());

    let content = "hello";
    let agent_fut = agent_loop.run(content, &context_builder);
    tokio::pin!(agent_fut);

    let _result = loop {
        tokio::select! {
            result = &mut agent_fut => {
                break result;
            }
            msg = grpc_client.recv_message() => {
                if let Some(_message) = msg {
                    // ❌ This should NOT compile: context_builder is already
                    //    immutably borrowed by agent_fut.
                    // context_builder.set_override_model("new-model".to_string());
                    //
                    // Expected compile error:
                    //   error[E0506]: cannot assign to `context_builder` because it is borrowed
                }
            }
        }
    };
}

/// Solution: Route ALL state mutations through inbound_tx
///
/// For messages that need `&mut agent_loop` or `&mut context_builder`,
/// we send them through the `inbound_tx` channel. The agent loop processes
/// them at iteration boundaries (already has an inner loop for
/// ContinueExecution/Interrupt).
///
/// This requires extending InboundMessage with new variants:
///   - InboundMessage::UpdateProvider { provider, model, ... }
///   - InboundMessage::ModelSwitch { model, provider }
///   - InboundMessage::WorkspaceContextUpdate { context_text }
///
/// For session management (create/activate/delete), we return "busy"
/// during agent execution — these don't make sense while the agent is running.
#[tokio::test]
async fn test_select_solution_route_through_inbound() {
    struct MockAgentLoop;
    impl MockAgentLoop {
        async fn run(&mut self, _msg: &str, _ctx: &ContextBuilder) -> Result<String, ()> {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            Ok("response".to_string())
        }
    }

    struct MockGrpcClient {
        rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    }
    impl MockGrpcClient {
        async fn recv_message(&mut self) -> Option<String> {
            self.rx.recv().await
        }
        async fn send_response(&mut self, _msg: &str) {}
        async fn send_intent(&mut self, _target: &str, _action: &str, _params: serde_json::Value, _broadcast: bool) {}
    }

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut agent_loop = MockAgentLoop;
    let mut grpc_client = MockGrpcClient { rx };
    let (inbound_tx, mut inbound_rx) = tokio::sync::mpsc::channel(16);

    // Send test messages
    tx.send("continue_execution".to_string()).unwrap();
    tx.send("llm_config_delivery".to_string()).unwrap();
    tx.send("list_sessions".to_string()).unwrap();
    tx.send("create_session".to_string()).unwrap();

    let content = "hello";
    let context_builder = ContextBuilder::new("system prompt".to_string());

    let agent_fut = agent_loop.run(content, &context_builder);
    tokio::pin!(agent_fut);

    let agent_result = loop {
        tokio::select! {
            result = &mut agent_fut => {
                break result;
            }
            msg = grpc_client.recv_message() => {
                let Some(message) = msg else { continue; };

                match message.as_str() {
                    // ✅ Route through inbound_tx — no borrow issues
                    "continue_execution" => {
                        let _ = inbound_tx.send(InboundMessage::ContinueExecution {
                            reason: "user_requested".to_string(),
                        }).await;
                    }
                    "interrupt" => {
                        let _ = inbound_tx.send(InboundMessage::Interrupt {
                            reason: "user_requested".to_string(),
                        }).await;
                    }
                    // ✅ Route through inbound_tx — agent loop applies update
                    //    at next iteration boundary
                    "llm_config_delivery" => {
                        // In the real implementation, this would be:
                        // inbound_tx.send(InboundMessage::UpdateProvider { ... }).await;
                        // For now, we demonstrate the pattern works
                        let _ = inbound_tx.send(InboundMessage::SystemNotification {
                            notification_type: "llm_config_update".to_string(),
                            data: serde_json::json!({"provider": "openai", "model": "gpt-4"}),
                        }).await;
                    }
                    // ✅ Direct handler — only needs &mut grpc_client (no &mut agent_loop)
                    "list_sessions" => {
                        grpc_client.send_response("session_list").await;
                    }
                    "get_session_messages" => {
                        grpc_client.send_response("messages").await;
                    }
                    // ✅ Reject during execution — session management doesn't make
                    //    sense while the agent loop is running
                    "create_session" | "activate_session" | "delete_session" => {
                        grpc_client.send_intent(
                            "desktop",
                            "error",
                            serde_json::json!({"error": "Agent busy — session management not available during execution"}),
                            false,
                        ).await;
                    }
                    _ => {}
                }
            }
        }
    };

    assert!(agent_result.is_ok());

    // Verify inbound messages were received
    let mut received = Vec::new();
    while let Ok(msg) = inbound_rx.try_recv() {
        received.push(msg);
    }
    assert_eq!(received.len(), 2, "Should have 2 inbound messages (continue + llm_config)");
}
