//! Linux: post-wake display signal via systemd-logind's `PrepareForSleep`.
//!
//! `org.freedesktop.login1.Manager.PrepareForSleep(b)` is a system-bus
//! signal that `systemd-logind` broadcasts on every sleep/wake cycle.
//! The boolean argument is `true` when the system is about to sleep
//! and `false` when it has just woken up. The false edge is the
//! Linux equivalent of Windows' `GUID_MONITOR_POWER_ON` (delivered
//! via the WndProc subclass in `win_wndproc.rs`) and macOS's
//! `NSWorkspaceDidWakeNotification` (delivered via
//! `macos_workspace.rs`): it is the right event to feed into
//! `wake_recovery::signal_display_ready()` so the post-wake webview
//! reload converges on the same code path as the other platforms.
//!
//! Without this signal source, Linux recovery falls back to the
//! cross-platform `Focused(true)` (gained only when the user clicks
//! the window) plus the 5 s `wake_recovery` timeout — both of which
//! converge correctly via the reload/verify/retry loop in
//! `recover_from_wake`, but the first reload lands a few seconds later
//! than on Windows. The logind `PrepareForSleep(false)` edge is what
//! closes that gap.
//!
//! # Why MatchRule and not a typed proxy
//!
//! A `#[zbus::proxy]`-derived struct would be more typesafe, but
//! using `MessageStream::for_match_rule` keeps this module free of
//! `zbus_macros` derive surface and matches the way other
//! long-lived Tauri components (the MQTT client, the power probe
//! loop) consume signals: a tokio task drains a `Stream` and
//! pattern-matches on each message. The single `bool` argument is
//! trivial to deserialize inline.
//!
//! # Why Connection::system() and not session
//!
//! `PrepareForSleep` is published on the **system** bus by
//! `systemd-logind` for all users. The session bus is the user
//! instance; it does not carry this signal. The desktop process is
//! expected to have system-bus access because it is a long-running
//! user GUI app — same as the tray icon, the clipboard reader, and
//! the D-Bus activation client that Tauri already uses.
//!
//! # Why `tokio::spawn` for the drain loop
//!
//! The function returns once the stream is constructed. A long-lived
//! task on the global tokio runtime (same one Tauri uses for the
//! power probe loop and the MQTT client) consumes the stream and
//! feeds `signal_display_ready()` on the false edge. The task
//! outlives the function call — when the runtime shuts down, the
//! task is cancelled and the D-Bus subscription is dropped, which
//! is the correct cleanup. We do not retain the task handle: any
//! failure mode (the bus disconnects, the proxy object disappears)
//! simply means no further signals arrive, and `recover_from_wake`
//! still converges via the 5 s timeout.

/// Install a long-lived `PrepareForSleep(false)` edge observer on
/// the system D-Bus. Returns `Err` only when the system D-Bus itself
/// is unreachable — connection refused, no `dbus-daemon` running,
/// etc. In that case the caller logs and continues; the
/// `Focused(true)` + 5 s timeout path in `recover_from_wake` still
/// works.
pub async fn install() -> Result<(), String> {
    use tokio_stream::StreamExt;
    use zbus::message::MessageType;
    use zbus::{Connection, MatchRule, MessageStream};

    let conn = Connection::system()
        .await
        .map_err(|e| format!("failed to connect to system D-Bus: {e}"))?;

    // Build a `MatchRule` that targets only the `PrepareForSleep`
    // signal on the logind Manager object. `path` and `interface`
    // are required because logind exposes many signals on the same
    // path; without them the stream would receive unrelated logind
    // signals (e.g. `SessionNew`, `UserNew`).
    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .path("/org/freedesktop/login1")
        .map_err(|e| format!("MatchRule path: {e}"))?
        .interface("org.freedesktop.login1.Manager")
        .map_err(|e| format!("MatchRule interface: {e}"))?
        .member("PrepareForSleep")
        .map_err(|e| format!("MatchRule member: {e}"))?
        .build();

    let mut stream = MessageStream::for_match_rule(rule, &conn, None)
        .await
        .map_err(|e| format!("failed to subscribe to PrepareForSleep: {e}"))?;

    tokio::spawn(async move {
        // The connection is moved into the task so the subscription
        // is kept alive for the whole task lifetime. Dropping `conn`
        // here would tear down the match-rule subscription.
        let _conn = conn;

        while let Some(msg) = stream.next().await {
            let Ok(msg) = msg else {
                tracing::warn!(
                    "[linux_logind] PrepareForSleep message error: {:?}",
                    msg.err()
                );
                continue;
            };

            // The signal body is exactly one boolean. `true` means
            // "the system is about to sleep" (sent BEFORE the sleep
            // begins, with a delay that gives apps a chance to
            // inhibit); `false` means "the system has just woken
            // up". We only care about the wake edge.
            let body = msg.body();
            let sleeping: bool = match body.deserialize() {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("[linux_logind] PrepareForSleep body deserialize failed: {e}");
                    continue;
                }
            };
            if !sleeping {
                crate::wake_recovery::signal_display_ready();
            }
        }

        tracing::warn!(
            "[linux_logind] PrepareForSleep stream ended; wake signal will be unavailable"
        );
    });

    tracing::info!(
        "Linux logind PrepareForSleep(false) signal observer installed (system D-Bus, org.freedesktop.login1.Manager)"
    );
    Ok(())
}
