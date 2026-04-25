//! Permission declaration and validation types
//!
//! Permissions follow a string-based format inspired by Android:
//! - `"network:https://api.weather.com"` — Network access with URL scope
//! - `"filesystem:read:~/Documents"` — Filesystem read with path scope
//! - `"filesystem:write:~/workdir"` — Filesystem write with path scope
//! - `"memory:read"` / `"memory:write"` — Memory access
//! - `"intent:send:com.example.calendar"` — Intent send
//! - `"intent:receive:com.example.weather"` — Intent receive
//! - `"identity:read"` / `"identity:write"` — Identity access
//! - `"shell"` — Shell command execution
//! - `"wasm"` — WASM tool execution

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
    /// Identity read access (query user identity fields)
    /// e.g., "identity:read"
    IdentityRead,
    /// Identity write access (store/update user identity fields)
    /// e.g., "identity:write"
    IdentityWrite,
    /// Shell command execution
    Shell,
    /// WASM tool execution
    Wasm,
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
        if s == "wasm" {
            return Some(Permission::Wasm);
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
            "identity" => match rest {
                "read" => Some(Permission::IdentityRead),
                "write" => Some(Permission::IdentityWrite),
                _ => None,
            },
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
            Permission::IdentityRead => "identity:read".to_string(),
            Permission::IdentityWrite => "identity:write".to_string(),
            Permission::Shell => "shell".to_string(),
            Permission::Wasm => "wasm".to_string(),
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
            (Permission::IdentityRead, Permission::IdentityRead) => true,
            (Permission::IdentityWrite, Permission::IdentityWrite) => true,
            (Permission::Shell, Permission::Shell) => true,
            (Permission::Wasm, Permission::Wasm) => true,
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
            Permission::IdentityRead | Permission::IdentityWrite => "identity",
            Permission::Shell => "shell",
            Permission::Wasm => "wasm",
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
            Permission::IdentityRead => PermissionRepr {
                perm_type: "IdentityRead".into(),
                value: None,
            },
            Permission::IdentityWrite => PermissionRepr {
                perm_type: "IdentityWrite".into(),
                value: None,
            },
            Permission::Shell => PermissionRepr {
                perm_type: "Shell".into(),
                value: None,
            },
            Permission::Wasm => PermissionRepr {
                perm_type: "Wasm".into(),
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
            "IdentityRead" => Ok(Permission::IdentityRead),
            "IdentityWrite" => Ok(Permission::IdentityWrite),
            "Shell" => Ok(Permission::Shell),
            "Wasm" => Ok(Permission::Wasm),
            other => Err(serde::de::Error::custom(format!(
                "Unknown permission type: {other}"
            ))),
        }
    }
}

// ── Permission Grant & Policy (S1.1) ─────────────────────────────────────

/// Record of a permission being granted to an Agent.
///
/// Tracks who authorized what, when, and with what scope.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionGrant {
    /// The Agent this grant belongs to
    pub agent_id: String,
    /// The permission that was granted
    pub permission: Permission,
    /// Who authorized this grant ("user", "system", or "auto")
    pub authorized_by: String,
    /// When the grant was created (Unix timestamp millis)
    pub granted_at: i64,
    /// Optional expiry time (Unix timestamp millis); None = permanent
    pub expires_at: Option<i64>,
    /// Scope constraint: e.g., a path or URL pattern further restricting the permission
    #[serde(default)]
    pub scope: Option<String>,
}

impl PermissionGrant {
    /// Create a new permanent permission grant
    pub fn new(agent_id: &str, permission: Permission, authorized_by: &str) -> Self {
        Self {
            agent_id: agent_id.to_string(),
            permission,
            authorized_by: authorized_by.to_string(),
            granted_at: chrono::Utc::now().timestamp_millis(),
            expires_at: None,
            scope: None,
        }
    }

    /// Create a grant with expiry
    pub fn with_expiry(agent_id: &str, permission: Permission, authorized_by: &str, expires_at: i64) -> Self {
        Self {
            expires_at: Some(expires_at),
            ..Self::new(agent_id, permission, authorized_by)
        }
    }

    /// Check if this grant has expired
    pub fn is_expired(&self) -> bool {
        match self.expires_at {
            Some(exp) => chrono::Utc::now().timestamp_millis() > exp,
            None => false,
        }
    }

    /// Check if this grant matches a requested permission
    ///
    /// A grant matches if: (1) it's not expired, and (2) the granted permission
    /// covers the requested permission (broad → narrow semantics).
    pub fn matches_request(&self, requested: &Permission) -> bool {
        !self.is_expired() && self.permission.matches(requested)
    }
}

/// Policy for how a permission category should be handled by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionPolicy {
    /// Auto-grant without asking the user (for low-risk permissions like memory:read)
    Allow,
    /// Auto-deny without asking the user
    Deny,
    /// Ask the user every time (for high-risk permissions like shell)
    AskAlways,
    /// Use the platform default policy
    Default,
}

impl PermissionPolicy {
    /// Get the default policy for a permission type
    pub fn for_permission(perm: &Permission) -> Self {
        match perm {
            // Low-risk: auto-grant
            Permission::MemoryRead => PermissionPolicy::Allow,
            Permission::IdentityRead => PermissionPolicy::Allow,
            Permission::IntentReceive(_) => PermissionPolicy::Allow,
            // High-risk: always ask
            Permission::Shell => PermissionPolicy::AskAlways,
            Permission::IdentityWrite => PermissionPolicy::AskAlways,
            Permission::Wasm => PermissionPolicy::AskAlways,
            // Medium-risk: ask on first use
            Permission::Network(_) => PermissionPolicy::Default,
            Permission::FilesystemRead(_) => PermissionPolicy::Default,
            Permission::FilesystemWrite(_) => PermissionPolicy::Default,
            Permission::MemoryWrite => PermissionPolicy::Default,
            Permission::IntentSend(_) => PermissionPolicy::Default,
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
        assert_eq!(Permission::IdentityRead.category(), "identity");
        assert_eq!(Permission::Shell.category(), "shell");
        assert_eq!(Permission::Wasm.category(), "wasm");
    }

    // ── PermissionGrant tests ──────────────────────────────────────────

    #[test]
    fn test_permission_grant_new() {
        let grant = PermissionGrant::new(
            "com.example.weather",
            Permission::Network(Some("https://api.weather.com".into())),
            "user",
        );
        assert_eq!(grant.agent_id, "com.example.weather");
        assert_eq!(grant.authorized_by, "user");
        assert!(grant.expires_at.is_none());
        assert!(grant.scope.is_none());
        assert!(!grant.is_expired());
    }

    #[test]
    fn test_permission_grant_with_expiry() {
        let past = chrono::Utc::now().timestamp_millis() - 10000;
        let expired_grant = PermissionGrant::with_expiry(
            "com.example.weather",
            Permission::Network(None),
            "user",
            past,
        );
        assert!(expired_grant.is_expired());

        let future = chrono::Utc::now().timestamp_millis() + 86400000; // 24h from now
        let valid_grant = PermissionGrant::with_expiry(
            "com.example.weather",
            Permission::Network(None),
            "user",
            future,
        );
        assert!(!valid_grant.is_expired());
    }

    #[test]
    fn test_permission_grant_matches_request() {
        let grant = PermissionGrant::new(
            "com.example.weather",
            Permission::Network(None), // broad: all network
            "user",
        );
        // Broad grant matches narrow request
        assert!(grant.matches_request(&Permission::Network(Some("https://api.weather.com".into()))));
        // Same type doesn't match different type
        assert!(!grant.matches_request(&Permission::Shell));
    }

    #[test]
    fn test_permission_grant_expired_does_not_match() {
        let past = chrono::Utc::now().timestamp_millis() - 10000;
        let expired = PermissionGrant::with_expiry(
            "com.example.weather",
            Permission::Network(None),
            "user",
            past,
        );
        // Expired grant does not match any request
        assert!(!expired.matches_request(&Permission::Network(None)));
    }

    #[test]
    fn test_permission_grant_serialization() {
        let grant = PermissionGrant::new(
            "com.example.weather",
            Permission::Shell,
            "user",
        );
        let json = serde_json::to_string(&grant).unwrap();
        let parsed: PermissionGrant = serde_json::from_str(&json).unwrap();
        assert_eq!(grant, parsed);
    }

    // ── PermissionPolicy tests ─────────────────────────────────────────

    #[test]
    fn test_permission_policy_for_low_risk() {
        assert_eq!(PermissionPolicy::for_permission(&Permission::MemoryRead), PermissionPolicy::Allow);
        assert_eq!(PermissionPolicy::for_permission(&Permission::IdentityRead), PermissionPolicy::Allow);
        assert_eq!(PermissionPolicy::for_permission(&Permission::IntentReceive(None)), PermissionPolicy::Allow);
    }

    #[test]
    fn test_permission_policy_for_high_risk() {
        assert_eq!(PermissionPolicy::for_permission(&Permission::Shell), PermissionPolicy::AskAlways);
        assert_eq!(PermissionPolicy::for_permission(&Permission::IdentityWrite), PermissionPolicy::AskAlways);
        assert_eq!(PermissionPolicy::for_permission(&Permission::Wasm), PermissionPolicy::AskAlways);
    }

    #[test]
    fn test_permission_policy_for_medium_risk() {
        assert_eq!(PermissionPolicy::for_permission(&Permission::Network(None)), PermissionPolicy::Default);
        assert_eq!(PermissionPolicy::for_permission(&Permission::FilesystemWrite(None)), PermissionPolicy::Default);
    }

    // ── New Permission type tests ─────────────────────────────────────

    #[test]
    fn test_permission_parse_identity() {
        let p = Permission::parse("identity:read").unwrap();
        assert_eq!(p, Permission::IdentityRead);

        let p2 = Permission::parse("identity:write").unwrap();
        assert_eq!(p2, Permission::IdentityWrite);
    }

    #[test]
    fn test_permission_parse_wasm() {
        let p = Permission::parse("wasm").unwrap();
        assert_eq!(p, Permission::Wasm);
    }

    #[test]
    fn test_identity_wasm_toml_roundtrip() {
        let perms = vec![
            Permission::IdentityRead,
            Permission::IdentityWrite,
            Permission::Wasm,
        ];
        let json_str = serde_json::to_string(&perms).unwrap();
        let parsed: Vec<Permission> = serde_json::from_str(&json_str).unwrap();
        assert_eq!(perms, parsed);
    }
}
