//! Per-instance user-preference overrides (avatar, builtin avatar,
//! display name).
//!
//! # Why a separate file from `manifest.toml`
//!
//! `manifest.*` is **package content**: the package author's default,
//! shipped inside the `.agent` archive and rewritten on every upgrade.
//! `overrides.*` is **user preference** for one installed instance.
//! Mixing them (the pre-ADR-073 behaviour) means a re-install silently
//! resets the user's avatar and display name.
//!
//! # Layout
//!
//! ```text
//! {packages_dir}/{agent_id}/
//!     {instance_id}/                     <- package dir: manifest.toml, assets/, skills/
//!     {instance_id}.overrides.json       <- this file (sibling: survives upgrade)
//! ```
//!
//! # Who touches it (ADR-009 §5 boundary)
//!
//! | role | access |
//! |------|--------|
//! | Runtime | **owner** — the only writer, resolves the effective avatar from it |
//! | Node | reads it **verbatim** and publishes it in `InstalledAgentInfo.overrides_json`; never parses it |
//! | Gateway | parses it for display (`list_agents` merge); never touches the file |
//!
//! A missing file (or a malformed one) means "no overrides": every
//! field falls back to the manifest.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// User-preference overrides for one installed agent instance.
///
/// Every field is optional; `None` means "no override — use the
/// manifest default". An empty string is **not** a valid value; writers
/// clear a field by removing the key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentOverrides {
    /// Custom avatar path relative to the package dir (e.g.
    /// `"assets/avatar-02.jpg"`). Takes priority over `builtin_avatar`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    /// Builtin avatar icon id (e.g. `"icon-05"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin_avatar: Option<String>,
    /// User-chosen display name, overriding `manifest.display_name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

impl AgentOverrides {
    /// Parse a JSON document. `None` / empty / malformed all mean "no
    /// overrides" — a corrupt preference file must never make an agent
    /// unlistable, so this deliberately swallows the error.
    pub fn from_json(raw: &str) -> Self {
        let raw = raw.trim();
        if raw.is_empty() {
            return Self::default();
        }
        serde_json::from_str(raw).unwrap_or_default()
    }

    /// True when no field is overridden (nothing to publish / merge).
    pub fn is_empty(&self) -> bool {
        self.avatar.is_none() && self.builtin_avatar.is_none() && self.display_name.is_none()
    }
}

/// Absolute path of the overrides file for an instance whose package
/// dir is `package_dir` (`{packages_dir}/{agent_id}/{instance_id}`).
///
/// Returns `None` when `package_dir` has no parent — i.e. the install
/// path is a bare directory name, which cannot be a valid ADR-073
/// layout anyway, so there is nowhere safe to put a sibling file.
pub fn overrides_path(package_dir: &Path, instance_id: &str) -> Option<PathBuf> {
    package_dir
        .parent()
        .map(|agent_dir| agent_dir.join(format!("{instance_id}.overrides.json")))
}

/// Extensions accepted for a user-supplied avatar file (ADR-017).
///
/// One list for every hop that validates the value — the Runtime's
/// `PUT /avatar-config`, the Node's upload + serve routes and the
/// package avatar-asset listing. A whitelist that exists in three
/// places drifts: one hop ends up admitting what another serves.
pub const AVATAR_EXTENSIONS: [&str; 6] = ["png", "jpg", "jpeg", "gif", "webp", "svg"];

/// True when `path` ends in one of [`AVATAR_EXTENSIONS`], ASCII
/// case-insensitively. Extension only — path-traversal checks belong
/// to the caller that resolves the file.
pub fn has_avatar_extension(path: &str) -> bool {
    match Path::new(path).extension().and_then(|e| e.to_str()) {
        Some(ext) => {
            let ext = ext.to_ascii_lowercase();
            AVATAR_EXTENSIONS.contains(&ext.as_str())
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_is_a_sibling_of_the_instance_dir() {
        let p = overrides_path(
            Path::new("/pkgs/com.acowork.foo/3f2a-uuid"),
            "3f2a-uuid",
        );
        assert_eq!(
            p,
            Some(PathBuf::from(
                "/pkgs/com.acowork.foo/3f2a-uuid.overrides.json"
            ))
        );
    }

    #[test]
    fn missing_or_malformed_json_degrades_to_no_overrides() {
        assert_eq!(AgentOverrides::from_json(""), AgentOverrides::default());
        assert_eq!(AgentOverrides::from_json("{not json"), AgentOverrides::default());
        let parsed = AgentOverrides::from_json(r#"{"display_name":"大鱼"}"#);
        assert_eq!(parsed.display_name.as_deref(), Some("大鱼"));
        assert!(parsed.avatar.is_none());
        assert!(!parsed.is_empty());
    }
}
