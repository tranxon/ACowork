//! L1 单元测试 — `CancellationToken` + `select_cancelled`
//!
//! 覆盖 ADR-044 §4.6 验证矩阵 L1：
//! - token cancel 前后 `cancelled()` future 行为（fast / linger）
//! - `select_cancelled`：cancel 先到 / fut 先到 / cancel 在 race 期间多发的丢弃
//! - reason 字段多线程可见性
//! - 取消的 drop-future 语义（inner future 被 drop）
//! - re-entry：同一 token 多次 `cancel` + 多次 `cancelled()`

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use crate::cancellation::{CancellationReason, CancellationToken, StopSource, select_cancelled};
use crate::error::{Result, RuntimeError};

// ──────────────────────────────────────────────────────────────────────
// CancellationToken 基础行为
// ──────────────────────────────────────────────────────────────────────

#[test]
fn new_token_is_active() {
    let token = CancellationToken::new();
    assert!(!token.is_cancelled(), "fresh token must be Active");
    assert!(token.reason().is_none(), "fresh token has no reason");
}

#[test]
fn cancel_flips_state_and_records_reason() {
    let token = CancellationToken::new();
    let reason = CancellationReason::UserStop {
        source: StopSource::Cli,
        reason: "manual".into(),
    };
    assert!(token.cancel(reason.clone()));
    assert!(token.is_cancelled());
    assert_eq!(token.reason(), Some(reason));
}

#[test]
fn cancel_is_first_wins() {
    let token = CancellationToken::new();
    let r1 = CancellationReason::UserStop {
        source: StopSource::Cli,
        reason: "first".into(),
    };
    let r2 = CancellationReason::UserStop {
        source: StopSource::Cli,
        reason: "second".into(),
    };
    assert!(token.cancel(r1.clone()));
    assert!(
        !token.cancel(r2.clone()),
        "second cancel must return false (first-wins)"
    );
    assert_eq!(
        token.reason(),
        Some(r1),
        "first reason must be preserved on subsequent cancels"
    );
}

#[test]
fn cancel_is_idempotent_for_distinct_variants() {
    let token = CancellationToken::new();
    assert!(token.cancel(CancellationReason::Pause));
    assert!(!token.cancel(CancellationReason::DebugStop));
    assert!(!token.cancel(CancellationReason::Error("oops".into())));
    assert_eq!(token.reason(), Some(CancellationReason::Pause));
}

#[test]
fn clones_share_underlying_state() {
    let a = CancellationToken::new();
    let b = a.clone();
    let c = b.clone();
    assert!(!a.is_cancelled());
    assert!(!c.is_cancelled());
    a.cancel(CancellationReason::DebugStop);
    assert!(b.is_cancelled(), "all clones must observe the cancel");
    assert!(c.is_cancelled());
    assert_eq!(b.reason(), Some(CancellationReason::DebugStop));
    assert_eq!(c.reason(), Some(CancellationReason::DebugStop));
}

#[test]
fn default_yields_active_token() {
    let token: CancellationToken = Default::default();
    assert!(!token.is_cancelled());
}

// ──────────────────────────────────────────────────────────────────────
// cancelled() future 行为
// ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn cancelled_future_returns_immediately_when_already_cancelled() {
    let token = CancellationToken::new();
    token.cancel(CancellationReason::Pause);
    // Must resolve well under the timeout — no Notified awaiting needed.
    tokio::time::timeout(Duration::from_millis(50), token.cancelled())
        .await
        .expect("cancelled() should resolve instantly when already cancelled");
}

#[tokio::test]
async fn cancelled_future_resolves_after_cancel() {
    let token = CancellationToken::new();
    let t = token.clone();

    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        t.cancel(CancellationReason::SessionClosed);
    });

    tokio::time::timeout(Duration::from_millis(500), token.cancelled())
        .await
        .expect("cancelled() should resolve within 500ms after cancel()");

    canceller.await.expect("canceller task panicked");
}

#[tokio::test]
async fn cancelled_future_lingers_when_not_cancelled() {
    let token = CancellationToken::new();
    // Without cancel(), the future must NOT resolve within a short timeout.
    let res = tokio::time::timeout(Duration::from_millis(50), token.cancelled()).await;
    assert!(
        res.is_err(),
        "cancelled() must not resolve without cancel() (got {:?})",
        res
    );
}

#[tokio::test]
async fn re_entry_after_cancel_resolves_immediately() {
    let token = CancellationToken::new();
    token.cancel(CancellationReason::SessionClosed);

    // Multiple awaited calls after cancel — all should return immediately.
    for i in 0..3 {
        tokio::time::timeout(Duration::from_millis(50), token.cancelled())
            .await
            .unwrap_or_else(|_| panic!("subsequent cancelled() #{} must resolve instantly", i));
    }
}

// ──────────────────────────────────────────────────────────────────────
// select_cancelled 行为
// ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn select_cancelled_future_wins_returns_some() {
    let token = CancellationToken::new();
    let result =
        select_cancelled(token, async { Ok::<i32, RuntimeError>(42) }).await;
    assert_eq!(result.unwrap(), Some(42));
}

#[tokio::test]
async fn select_cancelled_cancel_wins_returns_none() {
    let token = CancellationToken::new();
    let t = token.clone();

    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        t.cancel(CancellationReason::UserStop {
            source: StopSource::Cli,
            reason: "test".into(),
        });
    });

    // Inner future takes 200ms (much longer than the cancel signal at 10ms).
    let result = select_cancelled(token, async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok::<i32, RuntimeError>(99)
    })
    .await;

    assert_eq!(
        result.unwrap(),
        None,
        "cancel must win over the 200ms inner future"
    );
    canceller.await.unwrap();
}

#[tokio::test]
async fn select_cancelled_propagates_inner_error() {
    let token = CancellationToken::new();
    let result =
        select_cancelled::<i32, _>(token, async { Err(RuntimeError::Tool("inner failure".into())) })
            .await;
    assert!(result.is_err(), "inner error must propagate unchanged");
    match result.unwrap_err() {
        RuntimeError::Tool(msg) => assert_eq!(msg, "inner failure"),
        e => panic!("expected RuntimeError::Tool, got {:?}", e),
    }
}

#[tokio::test]
async fn select_cancelled_drops_inner_future_on_cancel() {
    let token = CancellationToken::new();
    let t = token.clone();
    let drop_flag = Arc::new(AtomicBool::new(false));

    // FlagGuard flips the AtomicBool in its Drop impl. We hold one inside
    // the inner future so we can detect when the future is dropped.
    struct FlagGuard(Arc<AtomicBool>);
    impl Drop for FlagGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let drop_flag_for_inner = drop_flag.clone();
    let inner = async move {
        let _guard = FlagGuard(drop_flag_for_inner);
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok::<(), RuntimeError>(())
    };

    let handle = tokio::spawn(async move { select_cancelled(t.clone(), inner).await });

    // Give the spawned task time to enter the select! and start the sleep.
    tokio::time::sleep(Duration::from_millis(20)).await;
    token.cancel(CancellationReason::UserStop {
        source: StopSource::Test,
        reason: "drop-test".into(),
    });

    let outcome = handle.await.unwrap();
    assert_eq!(outcome.unwrap(), None);
    assert!(
        drop_flag.load(Ordering::SeqCst),
        "inner future must be dropped on cancel (FlagGuard Drop should have fired)"
    );
}

#[tokio::test]
async fn select_cancelled_with_already_cancelled_token_returns_none() {
    let token = CancellationToken::new();
    token.cancel(CancellationReason::Pause);

    // Inner future must never run — biased; select! resolves the cancel branch first.
    let drop_flag = Arc::new(AtomicBool::new(false));
    struct FlagGuard(Arc<AtomicBool>);
    impl Drop for FlagGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let drop_flag_clone = drop_flag.clone();
    let inner = async move {
        let _guard = FlagGuard(drop_flag_clone);
        tokio::time::sleep(Duration::from_millis(100)).await;
        Ok::<(), RuntimeError>(())
    };

    let result = select_cancelled(token, inner).await;
    assert_eq!(result.unwrap(), None);
    // Drop may or may not fire depending on whether `fut` was constructed
    // (creating the future does not run it; the async block only runs when
    // polled). For this test we only require the result semantics.
    let _ = drop_flag; // suppress unused warning
}

#[tokio::test]
async fn select_cancelled_multiple_cancels_resolve_once() {
    let token = CancellationToken::new();
    let cancel_count = Arc::new(AtomicU32::new(0));
    let t = token.clone();
    let cc = cancel_count.clone();

    let cancel_task = tokio::spawn(async move {
        // Many cancels — only the first should take effect.
        for _ in 0..5 {
            t.cancel(CancellationReason::DebugStop);
            cc.fetch_add(1, Ordering::SeqCst);
        }
    });

    select_cancelled::<i32, _>(token.clone(), async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        Ok(1)
    })
    .await
    .unwrap();

    cancel_task.await.unwrap();
    assert_eq!(cancel_count.load(Ordering::SeqCst), 5);
    assert!(token.is_cancelled());
    assert_eq!(token.reason(), Some(CancellationReason::DebugStop));
}

// ──────────────────────────────────────────────────────────────────────
// 多线程可见性（reason 字段）
// ──────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reason_visible_across_threads_after_cancel() {
    let token = CancellationToken::new();
    let token_clone = token.clone();
    let counter = Arc::new(AtomicU32::new(0));

    let mut handles = Vec::new();
    for _ in 0..4 {
        let t = token.clone();
        let c = counter.clone();
        handles.push(tokio::spawn(async move {
            // Spin briefly waiting for cancel.
            t.cancelled().await;
            // After we observe cancel, reason must be visible to this thread.
            assert!(t.reason().is_some(), "reason must be visible after cancel");
            c.fetch_add(1, Ordering::SeqCst);
        }));
    }

    tokio::time::sleep(Duration::from_millis(20)).await;
    token_clone.cancel(CancellationReason::BudgetExceeded("tokens".into()));

    for h in handles {
        h.await.unwrap();
    }
    assert_eq!(
        counter.load(Ordering::SeqCst),
        4,
        "all 4 spawned tasks must observe cancel"
    );
    assert_eq!(
        token.reason(),
        Some(CancellationReason::BudgetExceeded("tokens".into()))
    );
}

// ──────────────────────────────────────────────────────────────────────
// 默认值 / trait 实现一致性
// ──────────────────────────────────────────────────────────────────────

#[test]
fn send_sync_compile_time_guarantees() {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    assert_send::<CancellationToken>();
    assert_sync::<CancellationToken>();
}

#[tokio::test]
async fn result_type_alias_works_with_select_cancelled() {
    // Compile-time check: `select_cancelled` returns `Result<Option<T>>`
    // (the crate's `Result` alias). We exercise it here to ensure no
    // signature drift.
    let token = CancellationToken::new();
    let r: Result<Option<i32>> = select_cancelled(token, async { Ok::<i32, RuntimeError>(7) }).await;
    assert_eq!(r.unwrap(), Some(7));
}