//! GrafeoStore — GrafeoDB-backed memory storage engine.

use std::path::Path;

use grafeo_engine::GrafeoDB;

use crate::error::Result;

/// Memory labels used in the RollBall memory system.
pub(crate) const MEMORY_LABELS: &[&str] = &[
    "Episodic",
    "Autobiographical",
    "Knowledge",
    "Procedural",
    "SystemConfig",
    "ToolInvocation",
    "Session",
];

/// Embedding vector dimension (384-dim, e.g. all-MiniLM-L6-v2).
const EMBEDDING_DIM: usize = 384;

/// Grafeo graph database backed by grafeo-engine.
pub struct GrafeoStore {
    /// Underlying GrafeoDB engine instance.
    pub(crate) db: GrafeoDB,
}

impl GrafeoStore {
    /// Open or create a persistent Grafeo database at the given path.
    ///
    /// Automatically initializes the schema (labels, vector indexes, text indexes).
    pub fn open(path: &Path) -> Result<Self> {
        let db = GrafeoDB::open(path)?;
        let store = Self { db };
        store.init_schema()?;
        Ok(store)
    }

    /// Create a new in-memory Grafeo database (useful for tests).
    ///
    /// Automatically initializes the schema.
    pub fn new_in_memory() -> Result<Self> {
        let db = GrafeoDB::new_in_memory();
        let store = Self { db };
        store.init_schema()?;
        Ok(store)
    }

    /// Close the database, flushing all pending writes.
    ///
    /// For persistent databases this ensures everything is safely on disk.
    pub fn close(&self) -> Result<()> {
        self.db.close().map_err(Into::into)
    }

    /// Initialize schema: create vector indexes and text indexes for all memory labels.
    fn init_schema(&self) -> Result<()> {
        for label in MEMORY_LABELS {
            // HNSW vector index on the "embedding" property.
            self.db.create_vector_index(
                label,
                "embedding",
                Some(EMBEDDING_DIM),
                Some("cosine"),
                None,
                None,
                None,
            )?;

            // BM25 text index on the "content" property.
            self.db.create_text_index(label, "content")?;
        }
        Ok(())
    }

    /// Return a reference to the underlying GrafeoDB.
    pub fn db(&self) -> &GrafeoDB {
        &self.db
    }
}
