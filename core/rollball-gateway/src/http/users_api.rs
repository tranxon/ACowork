//! User profile HTTP API handlers
//!
//! - GET  /api/users                  — list all user profiles
//! - POST /api/users                  — create a new user profile
//! - PUT  /api/users/{user_id}        — update a user profile
//! - POST /api/users/{user_id}/activate — switch active user

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
    routing::{get, post, put},
    Router,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::http::routes::{ApiError, AppState};
use crate::resource_cache;
use rollball_core::protocol::UserProfile;

/// Build the users router
pub fn users_routes() -> Router<AppState> {
    Router::new()
        .route("/api/users", get(list_users).post(create_user))
        .route("/api/users/{user_id}", put(update_user))
        .route("/api/users/{user_id}/activate", post(activate_user))
}

// ── Request types ──────────────────────────────────────────────────────

/// Request body for creating a new user
#[derive(Debug, Deserialize)]
pub struct CreateUserRequest {
    pub display_name: String,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub occupation: Option<String>,
    #[serde(default)]
    pub communication_style: Option<String>,
    #[serde(default)]
    pub custom: std::collections::HashMap<String, String>,
}

/// Request body for updating a user profile (all fields optional — merge)
#[derive(Debug, Deserialize, Default)]
pub struct UpdateUserRequest {
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub occupation: Option<String>,
    #[serde(default)]
    pub communication_style: Option<String>,
    #[serde(default)]
    pub custom: Option<std::collections::HashMap<String, String>>,
}

// ── Response types ─────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct UserListResponse {
    pub users: Vec<UserProfile>,
    pub version: u64,
}

#[derive(Debug, Serialize)]
pub struct UserResponse {
    pub user: UserProfile,
    pub version: u64,
}

#[derive(Debug, Serialize)]
pub struct ActivateResponse {
    pub active_user_id: String,
    pub version: u64,
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Build a data directory path from the AppState
async fn get_data_dir(state: &AppState) -> std::path::PathBuf {
    let gw = state.gateway_state.read().await;
    gw.config
        .as_ref()
        .map(|c| std::path::PathBuf::from(&c.data_dir))
        .unwrap_or_else(|| std::path::PathBuf::from("./data"))
}

// ── Handlers ───────────────────────────────────────────────────────────

/// `GET /api/users` — list all user profiles
pub async fn list_users(
    State(state): State<AppState>,
) -> Result<Json<UserListResponse>, (StatusCode, Json<ApiError>)> {
    let gw = state.gateway_state.read().await;
    let list = gw.resource_cache.user_profile_list.clone();
    Ok(Json(UserListResponse {
        users: list.users,
        version: list.version,
    }))
}

/// `POST /api/users` — create a new user profile
///
/// Generates a UUID v4, sets is_active=true (deactivates others),
/// bumps version, saves to disk, and hot-pushes to all running agents.
pub async fn create_user(
    State(state): State<AppState>,
    Json(req): Json<CreateUserRequest>,
) -> Result<(StatusCode, Json<UserResponse>), (StatusCode, Json<ApiError>)> {
    let now = now_iso();
    let user_id = Uuid::new_v4().to_string();

    let language = req.language.unwrap_or_else(|| "en-US".to_string());
    let timezone = req.timezone.unwrap_or_else(|| "UTC".to_string());

    let profile = UserProfile {
        user_id,
        display_name: req.display_name,
        language,
        timezone,
        city: req.city,
        country: req.country,
        occupation: req.occupation,
        communication_style: req.communication_style,
        custom: req.custom,
        created_at: now.clone(),
        updated_at: now,
        is_active: true,
    };

    // Update state: deactivate all others, add new user
    let data_dir = get_data_dir(&state).await;
    {
        let mut gw = state.gateway_state.write().await;
        // Deactivate all existing users
        for u in &mut gw.resource_cache.user_profile_list.users {
            u.is_active = false;
        }
        // Add new active user
        gw.resource_cache.user_profile_list.users.push(profile.clone());
    }

    // Bump version, save to disk
    {
        let mut gw = state.gateway_state.write().await;
        resource_cache::rebuild_and_save_user_profile_cache(&mut gw, &data_dir);
    }

    // Hot push to all running agents
    if let Some(pusher) = &state.pusher {
        pusher.push_user_profile().await;
    }

    let version = {
        let gw = state.gateway_state.read().await;
        gw.resource_cache.user_profile_list.version
    };

    tracing::info!(
        user_id = %profile.user_id,
        display_name = %profile.display_name,
        version = version,
        "User profile created"
    );

    Ok((StatusCode::CREATED, Json(UserResponse {
        user: profile,
        version,
    })))
}

/// `PUT /api/users/{user_id}` — update a user profile
///
/// Merges provided fields (None = keep existing), updates `updated_at`,
/// bumps version, saves to disk, and hot-pushes if the active user changed.
pub async fn update_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    Json(req): Json<UpdateUserRequest>,
) -> Result<Json<UserResponse>, (StatusCode, Json<ApiError>)> {
    let data_dir = get_data_dir(&state).await;

    let updated_profile = {
        let mut gw = state.gateway_state.write().await;
        let users = &mut gw.resource_cache.user_profile_list.users;
        let idx = users
            .iter()
            .position(|u| u.user_id == user_id)
            .ok_or_else(|| ApiError::not_found(&format!("User not found: {}", user_id)))?;

        let user = &mut users[idx];
        if let Some(name) = req.display_name {
            user.display_name = name;
        }
        if let Some(lang) = req.language {
            user.language = lang;
        }
        if let Some(tz) = req.timezone {
            user.timezone = tz;
        }
        if let Some(city) = req.city {
            user.city = Some(city);
        }
        if let Some(country) = req.country {
            user.country = Some(country);
        }
        if let Some(occ) = req.occupation {
            user.occupation = Some(occ);
        }
        if let Some(style) = req.communication_style {
            user.communication_style = Some(style);
        }
        if let Some(custom) = req.custom {
            user.custom = custom;
        }
        user.updated_at = now_iso();
        user.clone()
    };

    // Bump version, save to disk
    {
        let mut gw = state.gateway_state.write().await;
        resource_cache::rebuild_and_save_user_profile_cache(&mut gw, &data_dir);
    }

    // Hot push if the updated user is the active one
    if updated_profile.is_active {
        if let Some(pusher) = &state.pusher {
            pusher.push_user_profile().await;
        }
    }

    let version = {
        let gw = state.gateway_state.read().await;
        gw.resource_cache.user_profile_list.version
    };

    tracing::info!(
        user_id = %user_id,
        version = version,
        "User profile updated"
    );

    Ok(Json(UserResponse {
        user: updated_profile,
        version,
    }))
}

/// `POST /api/users/{user_id}/activate` — switch active user
///
/// Deactivates all users, activates the specified one, bumps version,
/// saves to disk, and hot-pushes to all running agents.
pub async fn activate_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
) -> Result<Json<ActivateResponse>, (StatusCode, Json<ApiError>)> {
    let data_dir = get_data_dir(&state).await;

    // Update state: deactivate all, activate target
    {
        let mut gw = state.gateway_state.write().await;
        let users = &mut gw.resource_cache.user_profile_list.users;

        // Verify user exists
        if !users.iter().any(|u| u.user_id == user_id) {
            return Err(ApiError::not_found(&format!("User not found: {}", user_id)));
        }

        // Deactivate all
        for u in users.iter_mut() {
            u.is_active = false;
        }
        // Activate target
        if let Some(target) = users.iter_mut().find(|u| u.user_id == user_id) {
            target.is_active = true;
            target.updated_at = now_iso();
        }
    }

    // Bump version, save to disk
    {
        let mut gw = state.gateway_state.write().await;
        resource_cache::rebuild_and_save_user_profile_cache(&mut gw, &data_dir);
    }

    // Hot push to all running agents
    if let Some(pusher) = &state.pusher {
        pusher.push_user_profile().await;
    }

    let version = {
        let gw = state.gateway_state.read().await;
        gw.resource_cache.user_profile_list.version
    };

    tracing::info!(
        active_user_id = %user_id,
        version = version,
        "Active user switched"
    );

    Ok(Json(ActivateResponse {
        active_user_id: user_id,
        version,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_users_routes_builds() {
        let _router = users_routes();
    }
}
