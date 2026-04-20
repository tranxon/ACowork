//! Permission declaration and validation types
//!
//! Permissions follow a string-based format inspired by Android:
//! - `"network:https://api.weather.com"` — Network access with URL scope
//! - `"filesystem:read:~/Documents"` — Filesystem read with path scope
//! - `"filesystem:write:~/workdir"` — Filesystem write with path scope
//! - `"memory:read"` / `"memory:write"` — Memory access
//! - `"intent:send:com.example.calendar"` — Intent send
//! - `"intent:receive:com.example.weather"` — Intent receive
//! - `"shell"` — Shell command execution

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Permission types that Agents can declare in manifest
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Permission {
    /// Network access with optional URL whitelist
    /// None = full network access granted; Some(url) = restricted to that URL
    /// e.g., "network:https://api.weather.com"
    Network(Option<String>),
    /// Filesystem read access with optional path restriction
    /// None = full filesystem read; Some(path) = restricted to that path
    /// e.g., "filesystem:read:~/Documents"
    FilesystemRead(Option<String>),
    /// Filesystem write access with optional path restriction
    /// None = full filesystem write; Some(path) = restricted to that path
    /// e.g., "filesystem:write:~/workdir"
    FilesystemWrite(Option<String>),
    /// Memory read access
    MemoryRead,
    /// Memory write access
    MemoryWrite,
    /// Intent send to specific agent
    /// e.g., "intent:send:com.example.calendar"
    IntentSend(Option<String>),
    /// Intent receive from specific agent
    /// e.g., "intent:receive:com.example.weather"
    IntentReceive(Option<String>),
    /// Shell command execution
    Shell,
}

impl Permission {
    /// Parse a permission string into a Permission enum
    ///
    /// # Examples
    /// ```
    /// use rollball_core::permission::Permission;
    /// let p = Permission::parse("network:https://api.weather.com").unwrap();
    /// assert!(matches!(p, Permission::Network(Some(_))));
    /// ```
    pub fn parse(s: &str) -> Option<Self> {
        // Handle simple single-word permissions first
        if s == "shell" {
            return Some(Permission::Shell);
        }

        // Split on the first colon only to get the category
        let (category, rest) = s.split_once(':')?;
        match category {
            "network" => Some(Permission::Network(Some(rest.to_string()))),
            "filesystem" => {
                // Split rest on first colon: "read:~/Documents" or "write:~/workdir"
                let (access, path) = rest.split_once(':')?;
                let path = Some(path.to_string());
                match access {
                    "read" => Some(Permission::FilesystemRead(path)),
                    "write" => Some(Permission::FilesystemWrite(path)),
                    _ => None,
                }
            }
            "memory" => match rest {
                "read" => Some(Permission::MemoryRead),
                "write" => Some(Permission::MemoryWrite),
                _ => None,
            },
            "intent" => {
                let (direction, target) = rest.split_once(':')?;
                let target = Some(target.to_string());
                match direction {
                    "send" => Some(Permission::IntentSend(target)),
                    "receive" => Some(Permission::IntentReceive(target)),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Convert permission to string representation
    pub fn to_permission_string(&self) -> String {
        match self {
            Permission::Network(Some(url)) => format!("network:{url}"),
            Permission::Network(None) => "network".to_string(),
            Permission::FilesystemRead(Some(path)) => format!("filesystem:read:{path}"),
            Permission::FilesystemRead(None) => "filesystem:read".to_string(),
            Permission::FilesystemWrite(Some(path)) => format!("filesystem:write:{path}"),
            Permission::FilesystemWrite(None) => "filesystem:write".to_string(),
            Permission::MemoryRead => "memory:read".to_string(),
            Permission::MemoryWrite => "memory:write".to_string(),
            Permission::IntentSend(Some(target)) => format!("intent:send:{target}"),
            Permission::IntentSend(None) => "intent:send".to_string(),
            Permission::IntentReceive(Some(source)) => format!("intent:receive:{source}"),
            Permission::IntentReceive(None) => "intent:receive".to_string(),
            Permission::Shell => "shell".to_string(),
        }
    }

    /// Check if this permission matches (covers) a requested permission.
    /// A broader permission (e.g., `Network(None)`) matches a narrower one
    /// (e.g., `Network(Some("https://api.weather.com"))`).
    ///
    /// Broad→narrow semantics: `Network(None)` = "all network access",
    /// so it covers any `Network(Some(_))`. Conversely, `Network(Some(url))`
    /// only covers the exact same URL or `Network(None)` is required for broader.
    pub fn matches(&self, requested: &Permission) -> bool {
        match (self, requested) {
            // Same type: broader scope (None) matches narrower scope (Some)
            (Permission::Network(None), Permission::Network(_)) => true,
            (Permission::Network(a), Permission::Network(b)) => a == b,
            (Permission::FilesystemRead(None), Permission::FilesystemRead(_)) => true,
            (Permission::FilesystemRead(a), Permission::FilesystemRead(b)) => a == b,
            (Permission::FilesystemWrite(None), Permission::FilesystemWrite(_)) => true,
            (Permission::FilesystemWrite(a), Permission::FilesystemWrite(b)) => a == b,
            (Permission::MemoryRead, Permission::MemoryRead) => true,
            (Permission::MemoryWrite, Permission::MemoryWrite) => true,
            (Permission::IntentSend(None), Permission::IntentSend(_)) => true,
            (Permission::IntentSend(a), Permission::IntentSend(b)) => a == b,
            (Permission::IntentReceive(None), Permission::IntentReceive(_)) => true,
            (Permission::IntentReceive(a), Permission::IntentReceive(b)) => a == b,
            (Permission::Shell, Permission::Shell) => true,
            _ => false,
        }
    }

    /// Get the category of this permission (e.g., "network", "filesystem")
    pub fn category(&self) -> &str {
        match self {
            Permission::Network(_) => "network",
            Permission::FilesystemRead(_) | Permission::FilesystemWrite(_) => "filesystem",
            Permission::MemoryRead | Permission::MemoryWrite => "memory",
            Permission::IntentSend(_) | Permission::IntentReceive(_) => "intent",
            Permission::Shell => "shell",
        }
    }
}

// Custom TOML serialization using tagged enum format
impl Serialize for Permission {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct PermissionRepr {
            #[serde(rename = "type")]
            perm_type: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            value: Option<String>,
        }

        let repr = match self {
            Permission::Network(v) => PermissionRepr {
                perm_type: "Network".into(),
                value: v.clone(),
            },
            Permission::FilesystemRead(v) => PermissionRepr {
                perm_type: "FilesystemRead".into(),
                value: v.clone(),
            },
            Permission::FilesystemWrite(v) => PermissionRepr {
                perm_type: "FilesystemWrite".into(),
                value: v.clone(),
            },
            Permission::MemoryRead => PermissionRepr {
                perm_type: "MemoryRead".into(),
                value: None,
            },
            Permission::MemoryWrite => PermissionRepr {
                perm_type: "MemoryWrite".into(),
                value: None,
            },
            Permission::IntentSend(v) => PermissionRepr {
                perm_type: "IntentSend".into(),
                value: v.clone(),
            },
            Permission::IntentReceive(v) => PermissionRepr {
                perm_type: "IntentReceive".into(),
                value: v.clone(),
            },
            Permission::Shell => PermissionRepr {
                perm_type: "Shell".into(),
                value: None,
            },
        };
        repr.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Permission {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct PermissionRepr {
            #[serde(rename = "type")]
            perm_type: String,
            #[serde(default)]
            value: Option<String>,
        }

        let repr = PermissionRepr::deserialize(deserializer)?;
        match repr.perm_type.as_str() {
            "Network" => Ok(Permission::Network(repr.value)),
            "FilesystemRead" => Ok(Permission::FilesystemRead(repr.value)),
            "FilesystemWrite" => Ok(Permission::FilesystemWrite(repr.value)),
            "MemoryRead" => Ok(Permission::MemoryRead),
            "MemoryWrite" => Ok(Permission::MemoryWrite),
            "IntentSend" => Ok(Permission::IntentSend(repr.value)),
            "IntentReceive" => Ok(Permission::IntentReceive(repr.value)),
            "Shell" => Ok(Permission::Shell),
            other => Err(serde::de::Error::custom(format!(
                "Unknown permission type: {other}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_permission_parse_network() {
        let p = Permission::parse("network:https://api.weather.com").unwrap();
        assert!(matches!(p, Permission::Network(Some(_))));
        assert_eq!(p.to_permission_string(), "network:https://api.weather.com");
    }

    #[test]
    fn test_permission_parse_filesystem() {
        let p = Permission::parse("filesystem:read:~/Documents").unwrap();
        assert!(matches!(p, Permission::FilesystemRead(Some(_))));

        let p2 = Permission::parse("filesystem:write:~/workdir").unwrap();
        assert!(matches!(p2, Permission::FilesystemWrite(Some(_))));
    }

    #[test]
    fn test_permission_parse_memory() {
        let p = Permission::parse("memory:read").unwrap();
        assert_eq!(p, Permission::MemoryRead);

        let p2 = Permission::parse("memory:write").unwrap();
        assert_eq!(p2, Permission::MemoryWrite);
    }

    #[test]
    fn test_permission_parse_shell() {
        let p = Permission::parse("shell").unwrap();
        assert_eq!(p, Permission::Shell);
    }

    #[test]
    fn test_permission_parse_intent() {
        let p = Permission::parse("intent:send:com.example.calendar").unwrap();
        assert!(matches!(p, Permission::IntentSend(Some(_))));

        let p2 = Permission::parse("intent:receive:com.example.weather").unwrap();
        assert!(matches!(p2, Permission::IntentReceive(Some(_))));
    }

    #[test]
    fn test_permission_parse_invalid() {
        assert!(Permission::parse("invalid").is_none());
        assert!(Permission::parse("filesystem:execute").is_none());
    }

    #[test]
    fn test_permission_matches_broad_narrow() {
        // Broad permission (no scope) matches narrow (with scope)
        let broad = Permission::Network(None);
        let narrow = Permission::Network(Some("https://api.weather.com".into()));
        assert!(broad.matches(&narrow));

        // Narrow doesn't match broad
        assert!(!narrow.matches(&broad));

        // Exact match
        let same = Permission::Network(Some("https://api.weather.com".into()));
        assert!(narrow.matches(&same));
    }

    #[test]
    fn test_permission_matches_different_types() {
        let network = Permission::Network(None);
        let shell = Permission::Shell;
        assert!(!network.matches(&shell));
    }

    #[test]
    fn test_permission_toml_roundtrip() {
        let perms = vec![
            Permission::Network(Some("https://api.weather.com".into())),
            Permission::MemoryRead,
            Permission::Shell,
        ];
        // Use JSON for Vec roundtrip (TOML array-of-tables format is verbose)
        let json_str = serde_json::to_string(&perms).unwrap();
        let parsed: Vec<Permission> = serde_json::from_str(&json_str).unwrap();
        assert_eq!(perms, parsed);
    }

    #[test]
    fn test_permission_category() {
        assert_eq!(Permission::Network(None).category(), "network");
        assert_eq!(Permission::FilesystemRead(None).category(), "filesystem");
        assert_eq!(Permission::MemoryRead.category(), "memory");
        assert_eq!(Permission::IntentSend(None).category(), "intent");
        assert_eq!(Permission::Shell.category(), "shell");
    }
}
