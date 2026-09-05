//! acowork-pm 统一错误类型。
//!
//! 所有 API handler / MCP tool 返回 `Result<T, PmError>`。
//! HTTP 层通过 `From<PmError> for axum::http::StatusCode` + JSON body 映射到 REST 状态码。
//!
//! 设计参考：[`docs/design/zh/21-pm-project-management.md`](../../docs/design/zh/21-pm-project-management.md) §5 错误码表。

use thiserror::Error;

/// PM 服务所有错误的统一枚举。
///
/// 实现原则：
/// - 每个错误变体携带**结构化上下文**（如 task_id），便于日志聚合与前端展示
/// - **不**实现 `Display` 的 `{}` 时打印敏感信息（task_id / path 都是非敏感的）
/// - HTTP 状态码映射见 [`http_status`] 方法
#[derive(Debug, Error)]
pub enum PmError {
    // ── 资源不存在 ────────────────────────────────────────────────────
    #[error("project not found: {0}")]
    ProjectNotFound(String),

    #[error("task not found: {0}")]
    TaskNotFound(String),

    #[error("attachment not found: {0}")]
    AttachmentNotFound(String),

    // ── 输入校验 ──────────────────────────────────────────────────────
    #[error("invalid id: {0}")]
    InvalidId(String),

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("path traversal detected for id: {0}")]
    PathTraversal(String),

    #[error("reserved name cannot be used as task id: {0}")]
    ReservedId(String),

    // ── 结构冲突 ──────────────────────────────────────────────────────
    #[error("cycle detected: cannot move {task_id} under {parent_id} (would create cycle)")]
    CycleDetected { task_id: String, parent_id: String },

    #[error("max task depth exceeded: {depth} (max {max})")]
    MaxDepthExceeded { depth: u8, max: u8 },

    #[error("too many children for task {0} (max 1000)")]
    TooManyChildren(String),

    // ── 依赖图 ────────────────────────────────────────────────────────
    #[error("dependency cycle detected via task {0}")]
    DependencyCycle(String),

    #[error("dependency not satisfied: task {task_id} is blocked by {blocker}")]
    DependencyNotSatisfied { task_id: String, blocker: String },

    // ── 附件 ──────────────────────────────────────────────────────────
    #[error("attachment too large: {size} bytes (max {max})")]
    AttachmentTooLarge { size: u64, max: u64 },

    #[error("attachment mime type not allowed: {0}")]
    AttachmentMimeRejected(String),

    #[error("too many attachments for task {0}")]
    TooManyAttachments(String),

    // ── 状态机 ────────────────────────────────────────────────────────
    #[error("invalid state transition for task {task_id}: {from} → {to}")]
    InvalidStateTransition {
        task_id: String,
        from: String,
        to: String,
    },

    // ── MCP 鉴权（设计 §9.2 / §9.3）──────────────────────────────────
    /// 调用者不是任务 assignee，或匿名调用被禁止的工具。
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// MCP 匿名连接调用需要身份的工具（无 `X-MCP-Actor`）。
    #[error("authentication required: {0}")]
    Unauthenticated(String),

    // ── 基础设施（自动 From 转换）───────────────────────────────────
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("multipart: {0}")]
    Multipart(String),

    #[error("image processing: {0}")]
    Image(String),

    // ── 兜底 ──────────────────────────────────────────────────────────
    #[error("internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, PmError>;

impl PmError {
    /// 映射到 HTTP 状态码。
    ///
    /// 与 [`docs/design/zh/21-pm-project-management.md`](../../docs/design/zh/21-pm-project-management.md) §5 错误码表 对齐。
    pub fn http_status(&self) -> u16 {
        match self {
            PmError::ProjectNotFound(_)
            | PmError::TaskNotFound(_)
            | PmError::AttachmentNotFound(_) => 404,

            PmError::InvalidId(_)
            | PmError::BadRequest(_)
            | PmError::PathTraversal(_)
            | PmError::ReservedId(_)
            | PmError::MaxDepthExceeded { .. }
            | PmError::TooManyChildren(_)
            | PmError::TooManyAttachments(_)
            | PmError::AttachmentTooLarge { .. }
            | PmError::AttachmentMimeRejected(_)
            | PmError::InvalidStateTransition { .. } => 400,

            PmError::CycleDetected { .. }
            | PmError::DependencyCycle(_) => 409,

            PmError::DependencyNotSatisfied { .. } => 409,

            PmError::Unauthenticated(_) => 401,

            PmError::Forbidden(_) => 403,

            PmError::Io(_) | PmError::Json(_) | PmError::Multipart(_) | PmError::Image(_) => 500,

            PmError::Internal(_) => 500,
        }
    }

    /// 客户端可见的机器可读错误码（snake_case，用于 API JSON body）。
    pub fn error_code(&self) -> &'static str {
        match self {
            PmError::ProjectNotFound(_) => "project_not_found",
            PmError::TaskNotFound(_) => "task_not_found",
            PmError::AttachmentNotFound(_) => "attachment_not_found",
            PmError::InvalidId(_) => "invalid_id",
            PmError::BadRequest(_) => "bad_request",
            PmError::PathTraversal(_) => "path_traversal",
            PmError::ReservedId(_) => "reserved_id",
            PmError::CycleDetected { .. } => "cycle_detected",
            PmError::MaxDepthExceeded { .. } => "max_depth_exceeded",
            PmError::TooManyChildren(_) => "too_many_children",
            PmError::DependencyCycle(_) => "dependency_cycle",
            PmError::DependencyNotSatisfied { .. } => "dependency_not_satisfied",
            PmError::AttachmentTooLarge { .. } => "attachment_too_large",
            PmError::AttachmentMimeRejected(_) => "attachment_mime_rejected",
            PmError::TooManyAttachments(_) => "too_many_attachments",
            PmError::InvalidStateTransition { .. } => "invalid_state_transition",
            PmError::Unauthenticated(_) => "unauthenticated",
            PmError::Forbidden(_) => "forbidden",
            PmError::Io(_) => "io_error",
            PmError::Json(_) => "json_error",
            PmError::Multipart(_) => "multipart_error",
            PmError::Image(_) => "image_error",
            PmError::Internal(_) => "internal_error",
        }
    }
}

// ─── axum integration ─────────────────────────────────────────────────────

impl axum::response::IntoResponse for PmError {
    fn into_response(self) -> axum::response::Response {
        use axum::Json;
        use serde_json::json;

        let status = axum::http::StatusCode::from_u16(self.http_status())
            .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);

        let body = json!({
            "error": {
                "code": self.error_code(),
                "message": self.to_string(),
            }
        });

        (status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 单点断言：每个 `PmError` 变体 → 期望的 (http_status, error_code)。
    ///
    /// 与 README §3 错误码表 & OpenAPI 一一对应。改一个错误必须同步改这里，
    /// 否则 handler 与文档漂移。
    #[test]
    fn http_status_and_code_mapping_table() {
        // (构造表达式, 期望 status, 期望 code)
        let cases: Vec<(PmError, u16, &'static str)> = vec![
            // ── 404 not found ─────────────────────────────────────────
            (PmError::ProjectNotFound("p-x".into()),       404, "project_not_found"),
            (PmError::TaskNotFound("t-x".into()),           404, "task_not_found"),
            (PmError::AttachmentNotFound("att-x".into()),   404, "attachment_not_found"),
            // ── 400 input validation ─────────────────────────────────
            (PmError::InvalidId("foo".into()),              400, "invalid_id"),
            (PmError::BadRequest("missing field".into()),   400, "bad_request"),
            (PmError::PathTraversal("../etc".into()),       400, "path_traversal"),
            (PmError::ReservedId("task.json".into()),       400, "reserved_id"),
            (PmError::MaxDepthExceeded { depth: 5, max: 4 },400, "max_depth_exceeded"),
            (PmError::TooManyChildren("t-x".into()),        400, "too_many_children"),
            (PmError::TooManyAttachments("t-x".into()),     400, "too_many_attachments"),
            (PmError::AttachmentTooLarge { size: 1, max: 0 }, 400, "attachment_too_large"),
            (PmError::AttachmentMimeRejected("exe".into()), 400, "attachment_mime_rejected"),
            (
                PmError::InvalidStateTransition {
                    task_id: "t-x".into(),
                    from: "done".into(),
                    to: "pending".into(),
                },
                400,
                "invalid_state_transition"
            ),
            // ── 409 conflicts ─────────────────────────────────────────
            (
                PmError::CycleDetected {
                    task_id: "a".into(),
                    parent_id: "b".into(),
                },
                409,
                "cycle_detected"
            ),
            (PmError::DependencyCycle("t-x".into()),        409, "dependency_cycle"),
            (
                PmError::DependencyNotSatisfied {
                    task_id: "t-y".into(),
                    blocker: "t-z".into(),
                },
                409,
                "dependency_not_satisfied"
            ),
            // ── 500 internal ──────────────────────────────────────────
            (
                PmError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "x")),
                500,
                "io_error"
            ),
            (
                PmError::Json(serde_json::from_str::<i32>("not json").unwrap_err()),
                500,
                "json_error"
            ),
            (PmError::Multipart("boundary missing".into()), 500, "multipart_error"),
            (PmError::Image("decode failed".into()),        500, "image_error"),
            // ── MCP auth（P3）────────────────────────────────────────
            (PmError::Unauthenticated("anonymous write".into()), 401, "unauthenticated"),
            (PmError::Forbidden("not assignee".into()),     403, "forbidden"),
            (PmError::Internal("x-actor missing".into()),   500, "internal_error"),
        ];

        let mut seen_codes = std::collections::HashSet::new();
        for (err, expected_status, expected_code) in &cases {
            assert_eq!(
                err.http_status(),
                *expected_status,
                "wrong status for {:?}",
                err
            );
            assert_eq!(err.error_code(), *expected_code, "wrong code for {:?}", err);
            // 防止同一 status 出现重复 code（不会发生，但显式断言更稳）
            assert!(
                seen_codes.insert(*expected_code),
                "duplicate error_code in test: {}",
                expected_code
            );
        }
        // 23 个错误码,与 README §3 表 23 行对齐
        // （P3 新增 unauthenticated / forbidden 两个 MCP 鉴权错误码）
        assert_eq!(seen_codes.len(), 23);
    }

    /// `IntoResponse` 生成的 JSON body 包含 code 与 message。
    #[tokio::test]
    async fn into_response_body_shape() {
        use axum::body::to_bytes;
        use axum::http::StatusCode;
        use axum::response::IntoResponse;

        let resp = PmError::TaskNotFound("t-abc".into()).into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let body_bytes = to_bytes(resp.into_body(), 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body["error"]["code"], "task_not_found");
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("t-abc"));
    }

    /// 显式断言 `axum::http::StatusCode::from_u16(self.http_status())` 不会失败。
    /// 防御性测试：如果将来有人把 http_status 改成返回 0 或 1000，这个测试会立即报警。
    #[test]
    fn http_status_is_valid_status_code() {
        use std::io;
        let all: Vec<PmError> = vec![
            PmError::ProjectNotFound("x".into()),
            PmError::TaskNotFound("x".into()),
            PmError::AttachmentNotFound("x".into()),
            PmError::InvalidId("x".into()),
            PmError::BadRequest("x".into()),
            PmError::PathTraversal("x".into()),
            PmError::ReservedId("x".into()),
            PmError::CycleDetected {
                task_id: "a".into(),
                parent_id: "b".into(),
            },
            PmError::MaxDepthExceeded { depth: 1, max: 0 },
            PmError::TooManyChildren("x".into()),
            PmError::DependencyCycle("x".into()),
            PmError::DependencyNotSatisfied {
                task_id: "a".into(),
                blocker: "b".into(),
            },
            PmError::AttachmentTooLarge { size: 1, max: 0 },
            PmError::AttachmentMimeRejected("x".into()),
            PmError::TooManyAttachments("x".into()),
            PmError::InvalidStateTransition {
                task_id: "t".into(),
                from: "a".into(),
                to: "b".into(),
            },
            PmError::Io(io::Error::other("x")),
            PmError::Multipart("x".into()),
            PmError::Image("x".into()),
            PmError::Internal("x".into()),
        ];
        for e in &all {
            let s = axum::http::StatusCode::from_u16(e.http_status());
            assert!(
                s.is_ok(),
                "PmError::{:?} mapped to invalid StatusCode {:?}",
                e,
                s
            );
        }
    }
}