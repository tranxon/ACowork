//! Dependency red line enforcement (ADR-055 §6.20).
//!
//! `acowork-node` must NEVER depend on `acowork-gateway`. The Gateway
//! carries 13 global modules of coupling; a dependency from the Node
//! on it would re-create the monolith in the new component and make
//! independent compilation/distribution impossible (the same reason
//! ADR-055 §5 rejected the "gateway cluster" option D).
//!
//! Mirrored on the Gateway side by
//! `acowork-gateway` `tests/node_dependency_redline.rs` (gateway must
//! not depend on acowork-node) and by a grep guard in `dev/ci.sh`.

use std::path::Path;

#[test]
fn node_crate_must_not_depend_on_gateway() {
    let manifest_dir = Path::env_or_default();
    let cargo_toml = manifest_dir.join("Cargo.toml");
    let content = std::fs::read_to_string(&cargo_toml)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", cargo_toml.display()));

    // Match dependency DECLARATION syntax (`acowork-gateway = ...`,
    // `acowork-gateway.workspace = ...`) rather than raw substring so
    // that doc comments mentioning the red line do not trip the guard.
    let declares_gateway = content
        .lines()
        .any(|line| {
            let trimmed = line.trim_start();
            (trimmed.starts_with("acowork-gateway") && trimmed.contains('='))
                || trimmed.starts_with("\"acowork-gateway\"")
        });

    assert!(
        !declares_gateway,
        "ADR-055 §6.20 dependency red line violated: \
         core/acowork-node/Cargo.toml declares a dependency on acowork-gateway"
    );

    // The allowed internal dependencies.
    assert!(content.contains("acowork-core"), "expected acowork-core dep");
    assert!(
        content.contains("acowork-mqtt-session"),
        "expected acowork-mqtt-session dep"
    );
}

trait EnvOrDefault {
    fn env_or_default() -> std::path::PathBuf;
}

impl EnvOrDefault for Path {
    fn env_or_default() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }
}
