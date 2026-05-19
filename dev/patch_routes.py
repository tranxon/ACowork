"""Patch routes.rs to add approval_pending field"""
import sys

path = r"d:\projects\rust\agent-study\core\rollball-gateway\src\http\routes.rs"
with open(path, "r", encoding="utf-8") as f:
    content = f.read()

# Fix 1: AppState::new() - add approval_pending
old1 = """            session_pending: session_pending.unwrap_or_else(|| {
                Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()))
            }),
            log_reload_handle: None,
            grpc_session_mgr: None,"""
new1 = """            session_pending: session_pending.unwrap_or_else(|| {
                Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()))
            }),
            approval_pending: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            log_reload_handle: None,
            grpc_session_mgr: None,"""

count1 = content.count(old1)
content = content.replace(old1, new1)
print(f"AppState::new(): found {count1} occurrence(s)")

# Fix 2: AppState::with_models_cache() - add approval_pending
old2 = """            session_pending: session_pending.unwrap_or_else(|| {
                Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()))
            }),
            log_reload_handle,
            grpc_session_mgr: None,"""
new2 = """            session_pending: session_pending.unwrap_or_else(|| {
                Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()))
            }),
            approval_pending: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            log_reload_handle,
            grpc_session_mgr: None,"""

count2 = content.count(old2)
content = content.replace(old2, new2)
print(f"AppState::with_models_cache(): found {count2} occurrence(s)")

with open(path, "w", encoding="utf-8") as f:
    f.write(content)
print("Done - routes.rs updated")
