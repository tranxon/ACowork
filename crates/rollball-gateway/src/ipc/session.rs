//! Connection session management
//!
//! Each connected Agent Runtime has a session with identity,
//! budget state, and message correlation.

use std::collections::HashMap;

/// Session state for a connected Agent Runtime
pub struct Session {
    /// Agent ID (set after KeyRelease handshake)
    pub agent_id: Option<String>,
    /// Pending request ID → correlation
    pub pending_requests: HashMap<u64, String>,
    /// Whether the session has been authenticated (KeyRelease completed)
    pub authenticated: bool,
    /// Request ID counter
    next_request_id: u64,
}

impl Session {
    /// Create a new unauthenticated session
    pub fn new() -> Self {
        Self {
            agent_id: None,
            authenticated: false,
            pending_requests: HashMap::new(),
            next_request_id: 1,
        }
    }

    /// Get next request ID
    pub fn next_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id += 1;
        id
    }

    /// Mark session as authenticated
    pub fn authenticate(&mut self, agent_id: &str) {
        self.agent_id = Some(agent_id.to_string());
        self.authenticated = true;
    }

    /// Check if session is authenticated
    pub fn is_authenticated(&self) -> bool {
        self.authenticated
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

/// Manages all active sessions
pub struct SessionManager {
    sessions: HashMap<String, Session>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }

    /// Create a new session for a connection
    pub fn create_session(&mut self, conn_id: &str) -> &mut Session {
        self.sessions.entry(conn_id.to_string())
            .or_insert_with(Session::new)
    }

    /// Get a session by connection ID
    pub fn get_session(&self, conn_id: &str) -> Option<&Session> {
        self.sessions.get(conn_id)
    }

    /// Get a mutable session by connection ID
    pub fn get_session_mut(&mut self, conn_id: &str) -> Option<&mut Session> {
        self.sessions.get_mut(conn_id)
    }

    /// Remove a session (on disconnect)
    pub fn remove_session(&mut self, conn_id: &str) -> Option<Session> {
        self.sessions.remove(conn_id)
    }

    /// Get count of active sessions
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Get count of authenticated sessions
    pub fn authenticated_count(&self) -> usize {
        self.sessions.values().filter(|s| s.authenticated).count()
    }

    /// Find session by agent_id
    pub fn find_by_agent_id(&self, agent_id: &str) -> Option<(&String, &Session)> {
        self.sessions.iter().find(|(_, s)| s.agent_id.as_deref() == Some(agent_id))
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_new() {
        let session = Session::new();
        assert!(session.agent_id.is_none());
        assert!(!session.authenticated);
    }

    #[test]
    fn test_session_authenticate() {
        let mut session = Session::new();
        session.authenticate("com.example.weather");
        assert_eq!(session.agent_id, Some("com.example.weather".to_string()));
        assert!(session.authenticated);
    }

    #[test]
    fn test_session_next_id() {
        let mut session = Session::new();
        assert_eq!(session.next_id(), 1);
        assert_eq!(session.next_id(), 2);
        assert_eq!(session.next_id(), 3);
    }

    #[test]
    fn test_session_manager_create() {
        let mut mgr = SessionManager::new();
        mgr.create_session("conn-1");
        assert_eq!(mgr.session_count(), 1);
    }

    #[test]
    fn test_session_manager_authenticate() {
        let mut mgr = SessionManager::new();
        mgr.create_session("conn-1");
        let session = mgr.get_session_mut("conn-1").unwrap();
        session.authenticate("com.example.weather");
        
        assert_eq!(mgr.authenticated_count(), 1);
    }

    #[test]
    fn test_session_manager_remove() {
        let mut mgr = SessionManager::new();
        mgr.create_session("conn-1");
        mgr.remove_session("conn-1");
        assert_eq!(mgr.session_count(), 0);
    }

    #[test]
    fn test_session_manager_find_by_agent_id() {
        let mut mgr = SessionManager::new();
        mgr.create_session("conn-1");
        mgr.get_session_mut("conn-1").unwrap().authenticate("com.example.weather");
        
        let result = mgr.find_by_agent_id("com.example.weather");
        assert!(result.is_some());
        
        let not_found = mgr.find_by_agent_id("com.example.unknown");
        assert!(not_found.is_none());
    }

    #[test]
    fn test_session_default() {
        let session = Session::default();
        assert!(!session.is_authenticated());
    }
}
