//! `SearchService` trait — cross-directory keyword search (design §4
//! `GET /api/search?keyword=`).
//!
//! The library has no global inverted index (design §3 "不维护全局树索
//! 引"), so search is a linear scan over every `library.json` + body
//! file. That is the correct trade-off at doc-library scale (thousands
//! of `.md` files); an index only becomes worth building when Desktop
//! search latency measurably hurts (rule-of-three / YAGNI).

use async_trait::async_trait;

use crate::error::Result;
use crate::types::SearchHit;

#[async_trait]
pub trait SearchService: Send + Sync {
    /// Case-insensitive keyword search over titles and bodies.
    ///
    /// Ranking: title match weights higher than content matches; results
    /// are sorted by descending score, then by name for determinism.
    async fn search(&self, keyword: &str, limit: usize) -> Result<Vec<SearchHit>>;
}
