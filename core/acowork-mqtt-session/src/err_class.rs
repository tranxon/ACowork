//! Error classification (ADR-039 §8.2 ErrClass).
//!
//! Maps MQTT connection errors into 6 categories with distinct
//! recovery policies. This module is rumqttc-version-agnostic:
//! callers build an [`ErrorDescriptor`] from their own error type
//! and [`classify`] maps it to [`ErrClass`].

use std::io;

/// A version-agnostic description of an MQTT connection error.
///
/// Both Runtime (rumqttc 0.24) and Desktop (rumqttc 0.25) build
/// this from their respective `ConnectionError` types, then pass
/// it to [`classify`]. This avoids coupling the shared crate to a
/// specific rumqttc version.
#[derive(Debug)]
pub struct ErrorDescriptor {
    /// What kind of error occurred.
    pub kind: ErrorKind,
    /// The underlying `io::ErrorKind` if available (for I/O errors).
    pub io_kind: Option<io::ErrorKind>,
}

/// High-level error category, independent of rumqttc version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// Network timeout (rumqttc `NetworkTimeout` / `FlushTimeout`).
    Timeout,
    /// I/O error from the TCP connection.
    Io,
    /// Broker refused the CONNECT (ConnAck return code != Success).
    ConnectionRefused(RefusedReason),
    /// Client-side state machine error.
    MqttState,
    /// Received an unexpected packet type.
    NotConnAck,
    /// Event loop requests channel exhausted.
    RequestsDone,
    /// TLS handshake or certificate error.
    Tls,
    /// WebSocket-related error.
    Websocket,
    /// Any other error not covered above.
    Other,
}

/// Why the broker refused the CONNECT packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefusedReason {
    /// Bad username or password.
    BadUserNamePassword,
    /// Client not authorized.
    NotAuthorized,
    /// Unsupported protocol version.
    RefusedProtocolVersion,
    /// Bad client ID.
    BadClientId,
    /// Broker service temporarily unavailable.
    ServiceUnavailable,
    /// Unknown refusal code.
    Unknown,
}

/// Classified error category.
///
/// | Class | Examples | Recovery |
/// |-------|----------|----------|
/// | E1 Transient | NetworkTimeout, Io(ECONNRESET) | Exponential backoff retry |
/// | E2 AuthRefused | ConnectionRefused(NotAuthorized / BadUserNamePassword) | Fatal – do not retry |
/// | E3 ProtocolRefused | ConnectionRefused(RefusedProtocolVersion / BadClientId) | Fatal – do not retry |
/// | E4 ConfigError | MqttState, Tls | Fatal – do not retry |
/// | E5 BrokerUnavailable | ConnectionRefused(ServiceUnavailable), Io(ECONNREFUSED) | Exponential backoff retry |
/// | E6 InternalBug | NotConnAck, RequestsDone | Fatal – do not retry |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrClass {
    /// E1: Transient network glitch – retry with backoff.
    Transient,
    /// E2: Authentication / authorization refused – fatal, do not retry.
    AuthRefused,
    /// E3: Protocol-level rejection – fatal, do not retry.
    ProtocolRefused,
    /// E4: Client configuration error – fatal, do not retry.
    ConfigError,
    /// E5: Broker temporarily unavailable – retry with backoff.
    BrokerUnavailable,
    /// E6: Internal bug or unexpected packet – fatal, do not retry.
    InternalBug,
}

impl ErrClass {
    /// Returns `true` if the error is retryable (E1 or E5).
    pub fn is_retryable(self) -> bool {
        matches!(self, ErrClass::Transient | ErrClass::BrokerUnavailable)
    }

    /// Returns `true` if the error is fatal (E2, E3, E4, E6).
    pub fn is_fatal(self) -> bool {
        !self.is_retryable()
    }

    /// Human-readable label for logging.
    pub fn label(self) -> &'static str {
        match self {
            ErrClass::Transient => "E1 Transient",
            ErrClass::AuthRefused => "E2 AuthRefused",
            ErrClass::ProtocolRefused => "E3 ProtocolRefused",
            ErrClass::ConfigError => "E4 ConfigError",
            ErrClass::BrokerUnavailable => "E5 BrokerUnavailable",
            ErrClass::InternalBug => "E6 InternalBug",
        }
    }
}

/// Classify an [`ErrorDescriptor`] into an [`ErrClass`].
///
/// This is the single entry point used by both Runtime and Desktop
/// event loops. Policy decisions (retry vs fatal) are derived from
/// the returned class, keeping classification and action separate.
pub fn classify(desc: &ErrorDescriptor) -> ErrClass {
    match desc.kind {
        // E1: Transient – network blips that typically resolve in
        // seconds.
        ErrorKind::Timeout => ErrClass::Transient,

        // E1/E5: I/O errors are split by kind.
        ErrorKind::Io => match desc.io_kind {
            // E5: Broker process not running / port not open.
            Some(io::ErrorKind::ConnectionRefused) => ErrClass::BrokerUnavailable,
            // E1: Transient network issues.
            Some(io::ErrorKind::ConnectionReset)
            | Some(io::ErrorKind::ConnectionAborted)
            | Some(io::ErrorKind::BrokenPipe)
            | Some(io::ErrorKind::TimedOut)
            | Some(io::ErrorKind::UnexpectedEof)
            | Some(io::ErrorKind::WouldBlock)
            | Some(io::ErrorKind::Interrupted) => ErrClass::Transient,
            // E1: Other I/O errors are conservatively transient.
            _ => ErrClass::Transient,
        },

        // E2/E3/E5: Broker refused the CONNECT packet.
        ErrorKind::ConnectionRefused(reason) => match reason {
            RefusedReason::BadUserNamePassword | RefusedReason::NotAuthorized => {
                ErrClass::AuthRefused
            }
            RefusedReason::RefusedProtocolVersion | RefusedReason::BadClientId => {
                ErrClass::ProtocolRefused
            }
            RefusedReason::ServiceUnavailable => ErrClass::BrokerUnavailable,
            RefusedReason::Unknown => ErrClass::BrokerUnavailable,
        },

        // E4: Client-side state machine error.
        ErrorKind::MqttState => ErrClass::ConfigError,

        // E4: TLS configuration errors.
        ErrorKind::Tls => ErrClass::ConfigError,

        // E1: WebSocket errors are often transient (proxy timeouts).
        ErrorKind::Websocket => ErrClass::Transient,

        // E6: Internal bugs.
        ErrorKind::NotConnAck => ErrClass::InternalBug,
        ErrorKind::RequestsDone => ErrClass::InternalBug,

        // E1: Unknown errors are conservatively transient.
        ErrorKind::Other => ErrClass::Transient,
    }
}

// ── Convenience builders from rumqttc ─────────────────────────
// rumqttc is a hard dependency (ADR-065 Step 3: MqttClient<B> needs its
// types), so From<&ConnectionError> is the single adapter — no private
// per-client adapters allowed (ADR-065 §5.6).

mod from_rumqttc_0_25 {
    use super::*;
    use rumqttc::ConnectionError;

    impl From<&ConnectionError> for ErrorDescriptor {
        fn from(err: &ConnectionError) -> Self {
            match err {
                ConnectionError::NetworkTimeout => ErrorDescriptor {
                    kind: ErrorKind::Timeout,
                    io_kind: None,
                },
                ConnectionError::FlushTimeout => ErrorDescriptor {
                    kind: ErrorKind::Timeout,
                    io_kind: None,
                },
                ConnectionError::Io(io_err) => ErrorDescriptor {
                    kind: ErrorKind::Io,
                    io_kind: Some(io_err.kind()),
                },
                ConnectionError::ConnectionRefused(code) => ErrorDescriptor {
                    kind: ErrorKind::ConnectionRefused(match code {
                        rumqttc::ConnectReturnCode::BadUserNamePassword => {
                            RefusedReason::BadUserNamePassword
                        }
                        rumqttc::ConnectReturnCode::NotAuthorized => RefusedReason::NotAuthorized,
                        rumqttc::ConnectReturnCode::RefusedProtocolVersion => {
                            RefusedReason::RefusedProtocolVersion
                        }
                        rumqttc::ConnectReturnCode::BadClientId => RefusedReason::BadClientId,
                        rumqttc::ConnectReturnCode::ServiceUnavailable => {
                            RefusedReason::ServiceUnavailable
                        }
                        _ => RefusedReason::Unknown,
                    }),
                    io_kind: None,
                },
                ConnectionError::MqttState(state_err) => {
                    // rumqttc wraps transient I/O errors (e.g.
                    // ConnectionAborted, ConnectionReset — both produced
                    // when the OS tears down the TCP socket during
                    // sleep/wake) inside MqttState. Two shapes exist:
                    //   StateError::Io(io::Error)                    — direct wrap
                    //   StateError::Deserialization(mqttbytes::Error::Io)
                    //     — the poll task read a partially-written
                    //     packet at wake time and mqttbytes wrapped the
                    //     same io::Error at decode time
                    // Unwrap the inner I/O error in BOTH cases instead
                    // of blindly classifying as ConfigError (fatal —
                    // breaks the event loop with a 60s backoff).
                    match state_err {
                        rumqttc::StateError::Io(io_err) => ErrorDescriptor {
                            kind: ErrorKind::Io,
                            io_kind: Some(io_err.kind()),
                        },
                        rumqttc::StateError::Deserialization(mqttbytes_err) => {
                            if let rumqttc::mqttbytes::Error::Io(io_err) = mqttbytes_err {
                                ErrorDescriptor {
                                    kind: ErrorKind::Io,
                                    io_kind: Some(io_err.kind()),
                                }
                            } else {
                                ErrorDescriptor {
                                    kind: ErrorKind::MqttState,
                                    io_kind: None,
                                }
                            }
                        }
                        _ => ErrorDescriptor {
                            kind: ErrorKind::MqttState,
                            io_kind: None,
                        },
                    }
                },
                ConnectionError::NotConnAck(_) => ErrorDescriptor {
                    kind: ErrorKind::NotConnAck,
                    io_kind: None,
                },
                ConnectionError::RequestsDone => ErrorDescriptor {
                    kind: ErrorKind::RequestsDone,
                    io_kind: None,
                },
                ConnectionError::Tls(_) => ErrorDescriptor {
                    kind: ErrorKind::Tls,
                    io_kind: None,
                },
                // Catch-all for websocket/proxy variants that are
                // behind feature flags not enabled by default.
                #[allow(unreachable_patterns)]
                _ => ErrorDescriptor {
                    kind: ErrorKind::Other,
                    io_kind: None,
                },
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io;

        #[test]
        fn mqtt_state_direct_io_error_is_transient() {
            let err = rumqttc::ConnectionError::MqttState(rumqttc::StateError::Io(
                io::Error::new(io::ErrorKind::ConnectionReset, "reset"),
            ));
            let desc = ErrorDescriptor::from(&err);
            assert_eq!(desc.kind, ErrorKind::Io);
            assert_eq!(classify(&desc), ErrClass::Transient);
        }

        #[test]
        fn mqtt_state_deserialization_io_error_is_transient() {
            // The sleep/wake shape: the poll task read a partially
            // written packet and mqttbytes wrapped the same io::Error
            // inside `StateError::Deserialization(mqttbytes::Error::Io)`.
            // It must NOT classify as fatal E4 ConfigError (which would
            // trigger the 60s fatal backoff on every wake).
            let err = rumqttc::ConnectionError::MqttState(
                rumqttc::StateError::Deserialization(rumqttc::mqttbytes::Error::Io(
                    io::Error::new(io::ErrorKind::ConnectionAborted, "aborted"),
                )),
            );
            let desc = ErrorDescriptor::from(&err);
            assert_eq!(desc.kind, ErrorKind::Io);
            assert_eq!(desc.io_kind, Some(io::ErrorKind::ConnectionAborted));
            assert_eq!(classify(&desc), ErrClass::Transient);
        }

        #[test]
        fn mqtt_state_other_variants_stay_config_error() {
            let err = rumqttc::ConnectionError::MqttState(rumqttc::StateError::InvalidState);
            let desc = ErrorDescriptor::from(&err);
            assert_eq!(desc.kind, ErrorKind::MqttState);
            assert_eq!(classify(&desc), ErrClass::ConfigError);
        }


        #[test]
        fn mqtt_state_io_econnreset_classified_transient_node_gateway_path() {
            // ADR-065 §7 #3 regression test. Node and Gateway previously
            // owned private `error_descriptor_from_rumqttc` adapters that
            // pattern-matched `MqttState(_)` without unwrapping the inner
            // `StateError::Io(ECONNRESET)`. The local match arm mapped
            // `ErrorKind::MqttState` straight to `ErrClass::ConfigError`
            // (E4 fatal), so every kernel TCP reset at OS wake — the
            // exact same ECONNRESET this test simulates — re-triggered
            // the 60-second fatal backoff and silently dropped start /
            // stop commands for a full minute.
            //
            // The shared `From<&ConnectionError>` adapter (this file)
            // unwraps `StateError::Io` and re-classifies the inner
            // `io::ErrorKind` as `ErrorKind::Io`, which `classify()`
            // maps to `ErrClass::Transient`. Every consumer (Desktop /
            // Node / Runtime / Gateway publisher) MUST go through this
            // adapter — enforced at CI level by the `ErrorKind::MqttState`
            // literal red line in `dev/ci.sh`.
            //
            // We construct an ECONNRESET via `io::ErrorKind::ConnectionReset`
            // (its cross-platform spelling — the raw OS code differs
            // between Linux / macOS / Windows but `ErrorKind` is portable).
            let io_err = io::Error::new(
                io::ErrorKind::ConnectionReset,
                "simulated kernel TCP reset at OS wake",
            );
            let err = rumqttc::ConnectionError::MqttState(rumqttc::StateError::Io(io_err));

            let desc = ErrorDescriptor::from(&err);
            // Must be unwrapped to ErrorKind::Io, NOT left as
            // ErrorKind::MqttState — that's the regression we guard.
            assert_eq!(
                desc.kind,
                ErrorKind::Io,
                "StateError::Io(ECONNRESET) must be unwrapped to ErrorKind::Io"
            );
            assert_eq!(
                desc.io_kind,
                Some(io::ErrorKind::ConnectionReset),
                "inner io::ErrorKind must be preserved so consumers can branch"
            );
            assert_eq!(
                classify(&desc),
                ErrClass::Transient,
                "ECONNRESET inside MqttState must be Transient (E1), not fatal E4 ConfigError — \
                 otherwise Node/Gateway hit the 60s fatal backoff on every wake"
            );
        }

    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    fn desc(kind: ErrorKind) -> ErrorDescriptor {
        ErrorDescriptor { kind, io_kind: None }
    }

    fn io_desc(kind: io::ErrorKind) -> ErrorDescriptor {
        ErrorDescriptor {
            kind: ErrorKind::Io,
            io_kind: Some(kind),
        }
    }

    #[test]
    fn classify_timeout_is_transient() {
        assert_eq!(classify(&desc(ErrorKind::Timeout)), ErrClass::Transient);
        assert!(classify(&desc(ErrorKind::Timeout)).is_retryable());
    }

    #[test]
    fn classify_conn_refused_broker_unavailable() {
        let d = desc(ErrorKind::ConnectionRefused(RefusedReason::ServiceUnavailable));
        assert_eq!(classify(&d), ErrClass::BrokerUnavailable);
        assert!(classify(&d).is_retryable());
    }

    #[test]
    fn classify_auth_refused_is_fatal() {
        let d = desc(ErrorKind::ConnectionRefused(RefusedReason::NotAuthorized));
        assert_eq!(classify(&d), ErrClass::AuthRefused);
        assert!(classify(&d).is_fatal());
    }

    #[test]
    fn classify_bad_password_is_fatal() {
        let d = desc(ErrorKind::ConnectionRefused(RefusedReason::BadUserNamePassword));
        assert_eq!(classify(&d), ErrClass::AuthRefused);
    }

    #[test]
    fn classify_protocol_version_is_fatal() {
        let d = desc(ErrorKind::ConnectionRefused(RefusedReason::RefusedProtocolVersion));
        assert_eq!(classify(&d), ErrClass::ProtocolRefused);
    }

    #[test]
    fn classify_bad_client_id_is_fatal() {
        let d = desc(ErrorKind::ConnectionRefused(RefusedReason::BadClientId));
        assert_eq!(classify(&d), ErrClass::ProtocolRefused);
    }

    #[test]
    fn classify_io_conn_refused_is_broker_unavailable() {
        assert_eq!(
            classify(&io_desc(io::ErrorKind::ConnectionRefused)),
            ErrClass::BrokerUnavailable
        );
        assert!(classify(&io_desc(io::ErrorKind::ConnectionRefused)).is_retryable());
    }

    #[test]
    fn classify_io_conn_reset_is_transient() {
        assert_eq!(
            classify(&io_desc(io::ErrorKind::ConnectionReset)),
            ErrClass::Transient
        );
    }

    #[test]
    fn classify_io_broken_pipe_is_transient() {
        assert_eq!(
            classify(&io_desc(io::ErrorKind::BrokenPipe)),
            ErrClass::Transient
        );
    }

    #[test]
    fn classify_requests_done_is_internal_bug() {
        assert_eq!(classify(&desc(ErrorKind::RequestsDone)), ErrClass::InternalBug);
        assert!(classify(&desc(ErrorKind::RequestsDone)).is_fatal());
    }

    #[test]
    fn classify_tls_is_config_error() {
        assert_eq!(classify(&desc(ErrorKind::Tls)), ErrClass::ConfigError);
    }

    #[test]
    fn classify_websocket_is_transient() {
        assert_eq!(classify(&desc(ErrorKind::Websocket)), ErrClass::Transient);
    }

    #[test]
    fn classify_mqtt_state_is_config_error() {
        assert_eq!(classify(&desc(ErrorKind::MqttState)), ErrClass::ConfigError);
    }

    #[test]
    fn err_class_labels() {
        assert_eq!(ErrClass::Transient.label(), "E1 Transient");
        assert_eq!(ErrClass::AuthRefused.label(), "E2 AuthRefused");
        assert_eq!(ErrClass::ProtocolRefused.label(), "E3 ProtocolRefused");
        assert_eq!(ErrClass::ConfigError.label(), "E4 ConfigError");
        assert_eq!(ErrClass::BrokerUnavailable.label(), "E5 BrokerUnavailable");
        assert_eq!(ErrClass::InternalBug.label(), "E6 InternalBug");
    }
}
