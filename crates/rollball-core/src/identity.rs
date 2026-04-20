//! User identity data structure

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// User identity information
///
/// TODO(Phase 2): add Zone and PrivacyLevel fields per design doc v3.4.
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
