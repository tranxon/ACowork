//! `GET /search` — single-agent aggregate search (ADR-081 §4.1).
//!
//! Aggregates the agent's memory / file / git scopes in-process by
//! locking the existing usecase slots and running each requested scope
//! in parallel (`tokio::join!`). The conversation scope (P1-2, vector
//! conversation index) is not built yet — it reports
//! `{"status":"not_indexed"}` and yields no hits.
//!
//! The Gateway reverse-proxies `/api/agents/{id}/search` to this route
//! (ADR-009: agent-private data stays inside the Runtime; the Gateway
//! never touches the filesystem and holds no search business logic —
//! ADR-064/070).

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::http::server::HttpState;
use crate::usecases::git_query::GitLogParams;
use crate::usecases::memory_query::SemanticMemoryQuery;
use crate::usecases::workspace_query::SearchFilesParams;

/// Query parameters for `GET /search` (ADR-081 §4.1).
#[derive(Debug, Deserialize)]
pub struct GlobalSearchParams {
    #[serde(default)]
    pub q: String,
    /// Comma-separated scope names: `conversation,memory,file,git`.
    #[serde(default)]
    pub scopes: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Semantic mode for the memory scope: `vector` | `hybrid` |
    /// `keyword`. Empty = `hybrid`.
    #[serde(default)]
    pub mode: String,
    /// Workspace id for the file / git scopes. When omitted those scopes
    /// are skipped (reported as `{"status":"skipped"}`) — the Desktop
    /// file/git tabs use their dedicated endpoints anyway.
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// ADR-081: `all` accepted for future cross-agent search; a
    /// single-agent Runtime has no cross-agent scope, so it is ignored.
    #[serde(default)]
    pub agent: Option<String>,
}

fn default_limit() -> usize {
    20
}

/// One ranked hit in the aggregate response.
#[derive(Debug, serde::Serialize)]
pub struct SearchHit {
    pub scope: String,
    pub title: String,
    pub snippet: String,
    pub score: f64,
    /// Scope-specific payload (see each scope builder).
    pub payload: Value,
}

/// Response body for `GET /search`.
#[derive(Debug, serde::Serialize)]
pub struct GlobalSearchResponse {
    pub hits: Vec<SearchHit>,
    /// Per-scope metadata: `{"memory": {"status","count"}, ...}`.
    pub scopes: Value,
    /// True while the (P1-2) conversation index is still being built.
    pub indexing: bool,
}

/// Embed `q` with the agent's live embedding provider (ADR-081 memory
/// scope). Returns `None` when no provider is bound or embedding fails —
/// the memory scope then falls back to BM25 text search.
pub(crate) async fn embed_query(state: &HttpState, q: &str) -> Option<Vec<f32>> {
    let provider: Option<Arc<dyn crate::embedding::EmbeddingProvider>> = {
        let guard = state.agent_core.read().ok();
        guard
            .as_ref()
            .and_then(|g| g.as_ref())
            .and_then(|c| c.embedding_provider.clone())
    };
    match provider {
        Some(provider) => match provider.embed(q).await {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!(error = %e, "global search: embedding failed, falling back to BM25");
                None
            }
        },
        None => None,
    }
}

/// Build `git`-scope hits with CommitPicker-identical semantics
/// (subject/author/short_hash/hash case-insensitive substring), applied
/// server-side as the ADR-081 sanctioned pre-filter. Pseudo-score keeps
/// newest-first order from `git log`.
fn git_scope_hits(commits: Vec<crate::usecases::git_query::GitCommitDto>, q: &str, limit: usize) -> Vec<SearchHit> {
    let lower = q.to_lowercase();
    commits
        .into_iter()
        .filter(|c| {
            c.subject.to_lowercase().contains(&lower)
                || c.author.to_lowercase().contains(&lower)
                || c.short_hash.contains(q)
                || c.hash.contains(q)
        })
        .take(limit)
        .enumerate()
        .map(|(i, c)| SearchHit {
            scope: "git".to_string(),
            title: c.subject.clone(),
            snippet: format!("{} · {}", c.author, c.date),
            score: 1.0 - (i as f64) * 1e-4,
            payload: json!({
                "hash": c.hash,
                "short_hash": c.short_hash,
                "author": c.author,
                "date": c.date,
            }),
        })
        .collect()
}

/// Map `ConversationIndex::search` hits to aggregate `SearchHit`s.
/// Each hit becomes one row; the displayed title is the session title
/// from `conversations/meta/{session_id}.json` when one exists, falling
/// back to the raw session id. The payload always carries the raw
/// `session_id` + `#message_index` for locate (ADR-081 §4.2).
fn conversation_hits_to_search_hits(
    hits: Vec<crate::conversation_index::ConversationHit>,
    titles: &std::collections::HashMap<String, Option<String>>,
) -> Vec<SearchHit> {
    hits.into_iter()
        .map(|h| {
            let title = titles
                .get(&h.session_id)
                .and_then(|t| t.clone())
                .unwrap_or_else(|| h.session_id.clone());
            SearchHit {
                scope: "conversation".to_string(),
                title,
                snippet: h.content.clone(),
                score: h.score,
                payload: json!({
                    "session_id": h.session_id,
                    "message_index": h.message_index,
                    "role": h.role,
                }),
            }
        })
        .collect()
}

/// Resolve `session_id → title` from `conversations/meta/*.json`
/// (blocking disk reads off the async runtime). Missing/unparseable
/// meta files map to `None` — the caller falls back to the raw id.
async fn resolve_session_titles(
    conversations_dir: &std::path::Path,
    session_ids: &[String],
) -> std::collections::HashMap<String, Option<String>> {
    let mut titles = std::collections::HashMap::with_capacity(session_ids.len());
    for sid in session_ids {
        let dir = conversations_dir.to_path_buf();
        let sid_key = sid.clone();
        let title = tokio::task::spawn_blocking(move || {
            crate::conversation::read_session_meta(&dir, &sid_key)
                .ok()
                .and_then(|m| m.title)
        })
        .await
        .unwrap_or(None);
        titles.insert(sid.clone(), title);
    }
    titles
}

/// `GET /search` handler.
pub(crate) async fn global_search(
    State(state): State<HttpState>,
    Query(params): Query<GlobalSearchParams>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let q = params.q.trim().to_string();
    let limit = params.limit.clamp(1, 50);
    let scopes: Vec<String> = params
        .scopes
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let empty = || {
        Json(json!({
            "hits": [],
            "scopes": {},
            "indexing": false,
        }))
    };
    if q.is_empty() || scopes.is_empty() {
        return Ok(empty());
    }

    let want_memory = scopes.iter().any(|s| s == "memory");
    let want_file = scopes.iter().any(|s| s == "file");
    let want_git = scopes.iter().any(|s| s == "git");
    let want_conversation = scopes.iter().any(|s| s == "conversation");

    // Clone service handles out of their slots so the guards are dropped
    // before the parallel await points below (also makes the futures Send).
    let memory_svc = if want_memory {
        state.memory_query.lock().await.clone()
    } else {
        None
    };
    let file_svc = if want_file {
        state.workspace_query.lock().await.clone()
    } else {
        None
    };
    let git_svc = if want_git {
        state.git_query.lock().await.clone()
    } else {
        None
    };
    let conversation_index = if want_conversation {
        state.conversation_index.read().ok().and_then(|g| g.clone())
    } else {
        None
    };
    let qq = q.clone();
    let mode = params.mode.clone();
    let memory_fut = async {
        let Some(svc) = memory_svc else {
            return (json!({"status": "unavailable"}), vec![]);
        };
        let embedding = embed_query(&state, &qq).await;
        let semantic_mode = if mode.is_empty() { "hybrid" } else { mode.as_str() };
        let query = SemanticMemoryQuery {
            query_text: qq,
            mode: semantic_mode.to_string(),
            limit,
            embedding,
        };
        match svc.semantic_search(&query).await {
            Ok(resp) => {
                let hits: Vec<SearchHit> = resp
                    .nodes
                    .into_iter()
                    .enumerate()
                    .map(|(i, n)| SearchHit {
                        scope: "memory".to_string(),
                        title: n.node_type.clone(),
                        snippet: n.content.clone(),
                        score: 1.0 - (i as f64) * 1e-4,
                        payload: json!({
                            "node_id": n.node_id,
                            "node_type": n.node_type,
                            "sub_type": n.sub_type,
                        }),
                    })
                    .collect();
                (
                    json!({"status": "ok", "count": resp.total}),
                    hits,
                )
            }
            Err(e) => (json!({"status": "error", "message": e.to_string()}), vec![]),
        }
    };

    let qq = q.clone();
    let workspace_id = params.workspace_id.clone();
    let file_fut = async {
        let Some(svc) = file_svc else {
            return (json!({"status": "unavailable"}), vec![]);
        };
        let Some(wid) = workspace_id.as_deref() else {
            return (json!({"status": "skipped", "message": "workspace_id required"}), vec![]);
        };
        let search_params = SearchFilesParams {
            q: Some(qq),
            workspace_id: Some(wid.to_string()),
            include: None,
            max_results: Some(limit),
            case_sensitive: false,
            whole_word: false,
        };
        match svc.search_files(&search_params).await {
            Ok(resp) => {
                let hits: Vec<SearchHit> = resp
                    .matches
                    .into_iter()
                    .enumerate()
                    .map(|(i, m)| SearchHit {
                        scope: "file".to_string(),
                        title: m.file.clone(),
                        snippet: m.text.clone(),
                        score: 1.0 - (i as f64) * 1e-4,
                        payload: json!({
                            "file": m.file,
                            "line": m.line,
                            "column": m.column,
                        }),
                    })
                    .collect();
                (
                    json!({"status": "ok", "count": resp.total_matches, "truncated": resp.truncated}),
                    hits,
                )
            }
            Err(e) => (json!({"status": "error", "message": e.to_string()}), vec![]),
        }
    };

    let qq = q.clone();
    let workspace_id = params.workspace_id.clone();
    let git_fut = async {
        let Some(svc) = git_svc else {
            return (json!({"status": "unavailable"}), vec![]);
        };
        let Some(wid) = workspace_id.as_deref() else {
            return (json!({"status": "skipped", "message": "workspace_id required"}), vec![]);
        };
        let log_params = GitLogParams {
            workspace_id: Some(wid.to_string()),
            path: None,
            limit: Some(100),
            skip: None,
        };
        match svc.log(&log_params).await {
            Ok(resp) => {
                let hits = git_scope_hits(resp.commits, &qq, limit);
                (json!({"status": "ok", "count": hits.len()}), hits)
            }
            Err(e) => (json!({"status": "error", "message": e.to_string()}), vec![]),
        }
    };

    let qq = q.clone();
    let conversation_fut = async {
        let Some(index) = conversation_index.as_ref() else {
            return (json!({"status": "not_indexed"}), vec![]);
        };
        let embedding = embed_query(&state, &qq).await;
        let hits = index.search(&qq, embedding.as_deref(), limit);
        // Session titles for display (dedup by session; meta reads off-runtime).
        let mut session_ids: Vec<String> = Vec::new();
        for h in &hits {
            if !session_ids.contains(&h.session_id) {
                session_ids.push(h.session_id.clone());
            }
        }
        let titles = resolve_session_titles(index.conversations_dir(), &session_ids).await;
        let hits = conversation_hits_to_search_hits(hits, &titles);
        (json!({"status": "ok", "count": hits.len()}), hits)
    };

    let ((memory_meta, memory_hits), (file_meta, file_hits), (git_meta, git_hits), (conversation_meta, conversation_hits)) =
        tokio::join!(memory_fut, file_fut, git_fut, conversation_fut);

    let mut hits: Vec<SearchHit> = Vec::new();
    hits.extend(memory_hits);
    hits.extend(file_hits);
    hits.extend(git_hits);
    hits.extend(conversation_hits);
    hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

    let indexing = conversation_index
        .as_ref()
        .map(|i| i.is_indexing())
        .unwrap_or(false);

    let mut scopes_map = Map::new();
    if want_conversation {
        scopes_map.insert("conversation".to_string(), conversation_meta);
    }
    if want_memory {
        scopes_map.insert("memory".to_string(), memory_meta);
    }
    if want_file {
        scopes_map.insert("file".to_string(), file_meta);
    }
    if want_git {
        scopes_map.insert("git".to_string(), git_meta);
    }

    Ok(Json(json!({
        "hits": hits,
        "scopes": scopes_map,
        "indexing": indexing,
    })))
}

#[cfg(test)]
mod tests {
    use crate::usecases::git_query::GitCommitDto;

    use super::{conversation_hits_to_search_hits, git_scope_hits};

    fn commit(subject: &str, author: &str, short: &str, hash: &str) -> GitCommitDto {
        GitCommitDto {
            hash: hash.to_string(),
            short_hash: short.to_string(),
            author: author.to_string(),
            date: "2025-01-01T00:00:00Z".to_string(),
            subject: subject.to_string(),
        }
    }

    #[test]
    fn git_scope_matches_any_of_four_fields_case_insensitively() {
        let commits = vec![
            commit("feat: add global search", "Alice", "a1b2c3", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            commit("fix: dark mode toggle", "Bob", "d4e5f6", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            commit("refactor: pm search", "Carol", "g7h8i9", "cccccccccccccccccccccccccccccccccccccccc"),
        ];
        // subject substring (case-insensitive)
        let hits = git_scope_hits(commits.clone(), "GLOBAL SEARCH", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].payload["short_hash"], "a1b2c3");
        // author substring
        assert_eq!(git_scope_hits(commits.clone(), "carol", 10).len(), 1);
        // short-hash substring
        assert_eq!(git_scope_hits(commits.clone(), "b2c", 10).len(), 1);
        // hash substring
        assert_eq!(git_scope_hits(commits.clone(), "bbbb", 10).len(), 1);
        // no match
        assert!(git_scope_hits(commits, "zebra", 10).is_empty());
    }

    #[test]
    fn git_scope_respects_limit() {
        let commits: Vec<GitCommitDto> = (0..5)
            .map(|i| commit(&format!("commit {i}"), "Alice", &format!("h{i}"), &format!("f{i}")))
            .collect();
        let hits = git_scope_hits(commits, "commit", 3);
        assert_eq!(hits.len(), 3);
        // Newest first preserved (commit 0 first in the seeded order).
        assert_eq!(hits[0].title, "commit 0");
    }

    #[test]
    fn conversation_hits_carry_session_locate_payload() {
        use crate::conversation_index::ConversationHit;
        let mut titles = std::collections::HashMap::new();
        titles.insert("s-abc".to_string(), Some("rust borrow deep-dive".to_string()));
        let hits = conversation_hits_to_search_hits(
            vec![
                ConversationHit {
                    session_id: "s-abc".to_string(),
                    message_index: 42,
                    role: "assistant".to_string(),
                    content: "borrow checker rules".to_string(),
                    score: 0.91,
                },
                ConversationHit {
                    session_id: "s-abc".to_string(),
                    message_index: 41,
                    role: "user".to_string(),
                    content: "what is lifetime elision".to_string(),
                    score: 0.8,
                },
                ConversationHit {
                    session_id: "s-unknown".to_string(),
                    message_index: 3,
                    role: "user".to_string(),
                    content: "no meta file".to_string(),
                    score: 0.5,
                },
            ],
            &titles,
        );
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].scope, "conversation");
        // Session title wins for display; raw id stays in the payload.
        assert_eq!(hits[0].title, "rust borrow deep-dive");
        assert_eq!(hits[0].snippet, "borrow checker rules");
        assert_eq!(hits[0].score, 0.91);
        assert_eq!(hits[0].payload["session_id"], "s-abc");
        assert_eq!(hits[0].payload["message_index"], 42);
        assert_eq!(hits[0].payload["role"], "assistant");
        assert_eq!(hits[1].payload["message_index"], 41);
        // Missing meta falls back to the raw session id.
        assert_eq!(hits[2].title, "s-unknown");
    }
}
