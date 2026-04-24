//! User identity data structures and trait definitions
//
//! Phase 2 (S3.2): IdentityCategory, PrivacyLevel, IdentityEntry,
//! IdentityStore trait, and IdentityObserver callback.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::error::Result;

// ============================================================================
// Enums
// ============================================================================

/// Category of identity information.
/// Used to group related identity fields for querying and access control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum IdentityCategory {
    /// Core identity: name, age, location
    Identity,
    /// User preferences and style
    Preferences,
    /// Knowledge and expertise
    Knowledge,
    /// Work-related information
    Work,
}

impl IdentityCategory {
    /// Returns the string representation used in storage and queries.
    pub fn as_str(&self) -> &'static str {
        match self {
            IdentityCategory::Identity => "Identity",
            IdentityCategory::Preferences => "Preferences",
            IdentityCategory::Knowledge => "Knowledge",
            IdentityCategory::Work => "Work",
        }
    }
}

impl std::str::FromStr for IdentityCategory {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "Identity" => Ok(IdentityCategory::Identity),
            "Preferences" => Ok(IdentityCategory::Preferences),
            "Knowledge" => Ok(IdentityCategory::Knowledge),
            "Work" => Ok(IdentityCategory::Work),
            _ => Err(format!("unknown IdentityCategory: {s}")),
        }
    }
}

/// Privacy level for identity information.
/// Determines who can access this data and how it's shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PrivacyLevel {
    /// Can be shared with any agent (e.g., preferred language)
    Public,
    /// Only shared with explicitly trusted agents (e.g., city, occupation)
    Personal,
    /// Never shared outside system agent (e.g., email, address)
    Sensitive,
}

impl PrivacyLevel {
    /// Returns the string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            PrivacyLevel::Public => "Public",
            PrivacyLevel::Personal => "Personal",
            PrivacyLevel::Sensitive => "Sensitive",
        }
    }
}

impl std::str::FromStr for PrivacyLevel {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "Public" => Ok(PrivacyLevel::Public),
            "Personal" => Ok(PrivacyLevel::Personal),
            "Sensitive" => Ok(PrivacyLevel::Sensitive),
            _ => Err(format!("unknown PrivacyLevel: {s}")),
        }
    }
}

// ============================================================================
// Structs
// ============================================================================

/// A single identity field entry with metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentityEntry {
    /// Field name (e.g., "display_name", "city", "language")
    pub field: String,
    /// Current value
    pub value: String,
    /// Confidence score [0.0, 1.0]
    pub confidence: f32,
    /// Category grouping
    pub category: IdentityCategory,
    /// Privacy level for sharing control
    pub privacy: PrivacyLevel,
    /// Source of this data ("onboarding", "user_input", "agent_question", "conversation")
    pub source: String,
    /// When this entry was last updated (ISO 8601)
    pub updated_at: String,
}

/// Result of an identity query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentityQueryResult {
    /// Requested fields with their values
    pub values: HashMap<String, String>,
    /// Confidence scores per field
    pub confidence: HashMap<String, f32>,
}

/// Subscription for identity field change notifications.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentitySubscription {
    /// Agent ID of the subscriber
    pub subscriber_id: String,
    /// Fields being watched
    pub fields: Vec<String>,
    /// Intent target for notifications
    pub callback_intent: String,
}

/// User identity information (legacy struct, kept for backward compatibility).
///
/// TODO(Phase 4): Migrate all uses to IdentityEntry-based storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    /// Unique user identifier
    pub user_id: String,
    /// Display name
    pub name: String,
    /// Email address
    #[serde(default)]
    pub email: Option<String>,
    /// Preferred language
    #[serde(default)]
    pub language: Option<String>,
    /// Timezone
    #[serde(default)]
    pub timezone: Option<String>,
    /// Custom attributes
    #[serde(default)]
    pub attributes: HashMap<String, String>,
}

// ============================================================================
// IdentityStore trait
// ============================================================================

/// Trait for identity storage backends.
///
/// Implementations store identity entries and manage subscriptions.
/// The primary implementation uses Grafeo (Autobiographical label) via
/// the System Agent's private GrafeoStore.
pub trait IdentityStore: Send + Sync {
    /// Store or update an identity entry.
    /// If the field already exists, its value and confidence are updated
    /// only if the new confidence >= existing confidence.
    fn store(&self, entry: &IdentityEntry) -> Result<()>;

    /// Query identity fields by name.
    /// Returns values and confidence scores for the requested fields.
    fn query(&self, fields: &[String]) -> Result<IdentityQueryResult>;

    /// Subscribe to changes on specific identity fields.
    /// When a subscribed field changes, the callback_intent receives
    /// an `identity:changed` notification.
    fn observe(&self, subscription: &IdentitySubscription) -> Result<()>;

    /// List all stored identity entries (for debugging/admin).
    fn list_all(&self) -> Result<Vec<IdentityEntry>>;
}

// ============================================================================
// Well-known identity field definitions
// ============================================================================

/// Well-known identity fields with their default categories and privacy levels.
pub struct IdentityFieldDef {
    /// Field name
    pub field: &'static str,
    /// Default category
    pub category: IdentityCategory,
    /// Default privacy level
    pub privacy: PrivacyLevel,
    /// Whether this field is required during onboarding
    pub required: bool,
}

/// All well-known identity field definitions.
pub const IDENTITY_FIELDS: &[IdentityFieldDef] = &[
    IdentityFieldDef { field: "display_name", category: IdentityCategory::Identity, privacy: PrivacyLevel::Public, required: true },
    IdentityFieldDef { field: "language", category: IdentityCategory::Preferences, privacy: PrivacyLevel::Public, required: true },
    IdentityFieldDef { field: "timezone", category: IdentityCategory::Preferences, privacy: PrivacyLevel::Public, required: true },
    IdentityFieldDef { field: "city", category: IdentityCategory::Identity, privacy: PrivacyLevel::Personal, required: false },
    IdentityFieldDef { field: "country", category: IdentityCategory::Identity, privacy: PrivacyLevel::Personal, required: false },
    IdentityFieldDef { field: "occupation", category: IdentityCategory::Work, privacy: PrivacyLevel::Personal, required: false },
    IdentityFieldDef { field: "communication_style", category: IdentityCategory::Preferences, privacy: PrivacyLevel::Public, required: false },
    IdentityFieldDef { field: "email", category: IdentityCategory::Identity, privacy: PrivacyLevel::Sensitive, required: false },
];

/// Look up a field definition by name.
pub fn find_field_def(field: &str) -> Option<&'static IdentityFieldDef> {
    IDENTITY_FIELDS.iter().find(|f| f.field == field)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identity_category_roundtrip() {
        for cat in [IdentityCategory::Identity, IdentityCategory::Preferences, IdentityCategory::Knowledge, IdentityCategory::Work] {
            let s = cat.as_str();
            let parsed: IdentityCategory = s.parse().unwrap();
            assert_eq!(parsed, cat);
        }
    }

    #[test]
    fn test_privacy_level_roundtrip() {
        for pl in [PrivacyLevel::Public, PrivacyLevel::Personal, PrivacyLevel::Sensitive] {
            let s = pl.as_str();
            let parsed: PrivacyLevel = s.parse().unwrap();
            assert_eq!(parsed, pl);
        }
    }

    #[test]
    fn test_identity_category_invalid() {
        assert!("Unknown".parse::<IdentityCategory>().is_err());
    }

    #[test]
    fn test_privacy_level_invalid() {
        assert!("Unknown".parse::<PrivacyLevel>().is_err());
    }

    #[test]
    fn test_find_field_def_known() {
        let def = find_field_def("display_name").unwrap();
        assert_eq!(def.field, "display_name");
        assert_eq!(def.category, IdentityCategory::Identity);
        assert_eq!(def.privacy, PrivacyLevel::Public);
        assert!(def.required);
    }

    #[test]
    fn test_find_field_def_unknown() {
        assert!(find_field_def("nonexistent_field").is_none());
    }

    #[test]
    fn test_identity_fields_required_count() {
        let required: Vec<_> = IDENTITY_FIELDS.iter().filter(|f| f.required).collect();
        assert_eq!(required.len(), 3, "display_name, language, timezone should be required");
    }

    #[test]
    fn test_identity_entry_serialization() {
        let entry = IdentityEntry {
            field: "city".to_string(),
            value: "Shanghai".to_string(),
            confidence: 0.85,
            category: IdentityCategory::Identity,
            privacy: PrivacyLevel::Personal,
            source: "user_input".to_string(),
            updated_at: "2026-04-24T00:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let restored: IdentityEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.field, entry.field);
        assert_eq!(restored.value, entry.value);
        assert!((restored.confidence - entry.confidence).abs() < f32::EPSILON);
        assert_eq!(restored.category, entry.category);
        assert_eq!(restored.privacy, entry.privacy);
    }

    #[test]
    fn test_identity_query_result_serialization() {
        let result = IdentityQueryResult {
            values: {
                let mut m = HashMap::new();
                m.insert("display_name".to_string(), "Zhang San".to_string());
                m.insert("city".to_string(), "Shanghai".to_string());
                m
            },
            confidence: {
                let mut m = HashMap::new();
                m.insert("display_name".to_string(), 1.0);
                m.insert("city".to_string(), 0.85);
                m
            },
        };
        let json = serde_json::to_string(&result).unwrap();
        let restored: IdentityQueryResult = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.values["display_name"], "Zhang San");
        assert!((restored.confidence["city"] - 0.85).abs() < f32::EPSILON);
    }
}
