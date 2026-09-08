//! Linux: post-wake display signal via `org.freedesktop.ScreenSaver.ActiveChanged`.
//!
//! `org.freedesktop.ScreenSaver.ActiveChanged(b)` is a session-bus
//! signal emitted by the desktop environment's screen-saver /
//! screen-lock service (GNOME, KDE, xscreensaver, ...) whenever the
//! screen saver — and, in practice, the display — activates or
//! deactivates. The boolean is `true` when the screen saver / display
//! sleep activates and `false` when it deactivates, i.e. when the
//! display has come back. The false edge feeds
//! `wake_recovery::signal_display_ready()` exactly like the other
//! platform display-wake sources.
//!
//! This complements `linux_logind.rs`'s `PrepareForSleep(false)`
//! observer: logind only fires on a full system suspend, so a pure
//! display sleep (DPMS off, system still running) — the display-sleep
//! case observed on macOS on 2026-09-08, where the WKWebView
//! compositor stalled on the wake edge and left only the translucent
//! background visible — has no logind signal. `ActiveChanged(false)`
//! is the Linux hook that covers that gap, mirroring the new macOS
//! CoreGraphics display-reconfiguration callback in
//! `macos_workspace.rs`.
//!
//! # Why MatchRule and not a typed proxy
//!
//! Same rationale as `linux_logind.rs`: a `MessageStream` +
//! `for_match_rule` keeps this module free of `zbus_macros` and
//! matches how the MQTT client and power probe loop consume signals.
//!
//! # Why no `path` in the match rule
//!
//! Unlike logind (a single well-known object path), the
//! `org.freedesktop.ScreenSaver` interface is implemented at
//! *different* object paths by different desktop environments
//! (GNOME/`gnome-session`, KDE/`kscreenlocker`, xscreensaver, ...).
//! The match rule deliberately matches on interface + member only, so
//! whichever DE is present gets picked up. A `path` filter would
//! silently miss the DEs that do not use the conventional path.
//!
//! # `ActiveChanged` vs lock/unlock
//!
//! The screen-saver signal fires for both display sleep and screen
//! lock. Unlocking therefore also produces a `false` edge and triggers
//! one conservative recovery reload. That is safe: the frontend
//! restores state from `sessionStorage` and the 15 s `reload_cooldown`
//! gate (see `lib.rs`) collapses any burst of lock/unlock edges into a
//! single reload. The cost of one unnecessary reload on unlock is
//! negligible compared to the risk of a frozen compositor after a
//! display sleep.
//!
//! # Why `tokio::spawn` for the drain loop
//!
//! Identical to `linux_logind.rs`: the task is cancelled when the
//! runtime shuts down, dropping the D-Bus subscription. Any failure
//! (bus disconnect, DE without the interface) just means no further
//! signals arrive and `recover_from_wake` still converges via its
//! `Focused(true)` + 5 s timeout path.

/// Install a long-lived `ActiveChanged(false)` edge observer on the
/// session D-Bus. Returns `Err` only when the session D-Bus itself is
/// unreachable. If the desktop environment does not implement the
/// `org.freedesktop.ScreenSaver` interface, the subscription simply
/// receives no signals — the caller's logind `PrepareForSleep` path
/// and the `Focused(true)` + 5 s timeout still work, so this is a
/// pure best-effort signal source.
pub async fn install() -> Result<(), String> {
    use tokio_stream::StreamExt;
    use zbus::message::MessageType;
    use zbus::{Connection, MatchRule, MessageStream};

    let conn = Connection::session()
        .await
        .map_err(|e| format!("failed to connect to session D-Bus: {e}"))?;

    // Match on interface + member only (see module docs for why the
    // object path is intentionally left unconstrained).
    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .interface("org.freedesktop.ScreenSaver")
        .map_err(|e| format!("MatchRule interface: {e}"))?
        .member("ActiveChanged")
        .map_err(|e| format!("MatchRule member: {e}"))?
        .build();

    let mut stream = MessageStream::for_match_rule(rule, &conn, None)
        .await
        .map_err(|e| format!("failed to subscribe to ActiveChanged: {e}"))?;

    tokio::spawn(async move {
        // The connection is moved into the task so the subscription
        // is kept alive for the whole task lifetime. Dropping `conn`
        // here would tear down the match-rule subscription.
        let _conn = conn;

        while let Some(msg) = stream.next().await {
            let Ok(msg) = msg else {
                tracing::warn!(
                    "[linux_screensaver] ActiveChanged message error: {:?}",
                    msg.err()
                );
                continue;
            };

            // The signal body is exactly one boolean. `true` means the
            // screen saver / display sleep is activating; `false`
            // means it just deactivated — the display is back. We only
            // care about the false (wake) edge.
            let body = msg.body();
            let active: bool = match body.deserialize() {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(
                        "[linux_screensaver] ActiveChanged body deserialize failed: {e}"
                    );
                    continue;
                }
            };
            if !active {
                crate::wake_recovery::signal_display_ready();
            }
        }

        tracing::warn!(
            "[linux_screensaver] ActiveChanged stream ended; display-wake signal will be unavailable"
        );
    });

    tracing::info!(
        "Linux ScreenSaver ActiveChanged(false) signal observer installed (session D-Bus, org.freedesktop.ScreenSaver)"
    );
    Ok(())
}
