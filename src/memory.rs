//! Local, SQLite-backed semantic memory for Yo.
//!
//! The module deliberately owns no connection. Callers initialize and operate on
//! a `rusqlite::Connection`, which keeps it usable from the CLI, a future TUI,
//! tests, and migration tools without introducing global state.

use rusqlite::{params, Connection, OptionalExtension, Row, Transaction};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

pub const MEMORY_SCHEMA_VERSION: i64 = 2;
const DEFAULT_LIMIT: usize = 50;
const DEFAULT_CANDIDATE_LIMIT: usize = 64;
const MAX_QUERY_LIMIT: usize = 1_000;
const RRF_K: f64 = 60.0;
const WEEK_SECONDS: i64 = 7 * 24 * 60 * 60;
const MONTH_SECONDS: i64 = 30 * 24 * 60 * 60;
const MAX_MEMORY_JOB_ATTEMPTS: u32 = 3;

pub type MemoryResult<T> = Result<T, MemoryError>;

#[derive(Debug)]
pub enum MemoryError {
    Database(rusqlite::Error),
    InvalidInput(String),
    DuplicateMemory { existing_id: i64 },
    NotFound { entity: &'static str, id: i64 },
    UnsupportedSchema { found: i64, supported: i64 },
    CorruptEmbedding { memory_id: i64, reason: String },
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(f, "memory database error: {error}"),
            Self::InvalidInput(message) => write!(f, "invalid memory input: {message}"),
            Self::DuplicateMemory { existing_id } => {
                write!(f, "memory duplicates existing memory {existing_id}")
            }
            Self::NotFound { entity, id } => write!(f, "{entity} {id} was not found"),
            Self::UnsupportedSchema { found, supported } => write!(
                f,
                "memory schema version {found} is newer than supported version {supported}"
            ),
            Self::CorruptEmbedding { memory_id, reason } => {
                write!(f, "memory {memory_id} has a corrupt embedding: {reason}")
            }
        }
    }
}

impl std::error::Error for MemoryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for MemoryError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Database(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemoryScope {
    Global,
    Repo,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemorySensitivity {
    Normal,
    Sensitive,
}

impl MemorySensitivity {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Sensitive => "sensitive",
        }
    }

    fn from_db(value: String) -> rusqlite::Result<Self> {
        match value.as_str() {
            "normal" => Ok(Self::Normal),
            "sensitive" => Ok(Self::Sensitive),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                format!("unknown memory sensitivity {other:?}").into(),
            )),
        }
    }
}

impl MemoryScope {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Repo => "repo",
        }
    }

    fn from_db(value: String) -> rusqlite::Result<Self> {
        match value.as_str() {
            "global" => Ok(Self::Global),
            "repo" => Ok(Self::Repo),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                format!("unknown memory scope {other:?}").into(),
            )),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Embedding {
    pub model: String,
    pub vector: Vec<f32>,
}

impl Embedding {
    pub fn new(model: impl Into<String>, vector: Vec<f32>) -> Self {
        Self {
            model: model.into(),
            vector,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct NewMemory {
    pub text: String,
    pub kind: String,
    pub scope: MemoryScope,
    pub repo: Option<String>,
    pub pinned: bool,
    pub importance: f64,
    pub confidence: f64,
    pub sensitivity: MemorySensitivity,
    pub expires_at: Option<i64>,
    pub source_message_id: Option<i64>,
    pub embedding: Option<Embedding>,
}

impl NewMemory {
    pub fn global(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: "fact".to_string(),
            scope: MemoryScope::Global,
            repo: None,
            pinned: false,
            importance: 0.5,
            confidence: 1.0,
            sensitivity: MemorySensitivity::Normal,
            expires_at: None,
            source_message_id: None,
            embedding: None,
        }
    }

    pub fn repo(text: impl Into<String>, repo: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: "fact".to_string(),
            scope: MemoryScope::Repo,
            repo: Some(repo.into()),
            pinned: false,
            importance: 0.5,
            confidence: 1.0,
            sensitivity: MemorySensitivity::Normal,
            expires_at: None,
            source_message_id: None,
            embedding: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Memory {
    pub id: i64,
    pub text: String,
    pub normalized_text: String,
    pub kind: String,
    pub scope: MemoryScope,
    pub repo: Option<String>,
    pub pinned: bool,
    pub importance: f64,
    pub confidence: f64,
    pub sensitivity: MemorySensitivity,
    pub expires_at: Option<i64>,
    pub source_message_id: Option<i64>,
    pub superseded_by: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_accessed_at: Option<i64>,
    pub access_count: u64,
    pub embedding_model: Option<String>,
    pub embedding_dimensions: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddMemoryOutcome {
    pub id: i64,
    pub inserted: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum EmbeddingUpdate {
    #[default]
    Keep,
    Remove,
    Replace(Embedding),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MemoryUpdate {
    pub text: Option<String>,
    pub kind: Option<String>,
    pub scope: Option<MemoryScope>,
    pub repo: Option<Option<String>>,
    pub pinned: Option<bool>,
    pub importance: Option<f64>,
    pub confidence: Option<f64>,
    pub sensitivity: Option<MemorySensitivity>,
    pub expires_at: Option<Option<i64>>,
    pub source_message_id: Option<Option<i64>>,
    pub embedding: EmbeddingUpdate,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListScope {
    All,
    Global,
    Repo(String),
    RepoAndGlobal(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListOptions {
    pub scope: ListScope,
    pub include_inactive: bool,
    pub limit: usize,
    pub offset: usize,
}

impl Default for ListOptions {
    fn default() -> Self {
        Self {
            scope: ListScope::All,
            include_inactive: false,
            limit: DEFAULT_LIMIT,
            offset: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClearScope {
    All,
    Global,
    Repo(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct MemoryQuery {
    pub text: String,
    pub embedding: Option<Embedding>,
    pub repo: Option<String>,
    pub include_global: bool,
    pub limit: usize,
    pub candidate_limit: usize,
}

impl MemoryQuery {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            embedding: None,
            repo: None,
            include_global: true,
            limit: 10,
            candidate_limit: DEFAULT_CANDIDATE_LIMIT,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct VectorMatch {
    pub memory: Memory,
    pub similarity: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MemorySearchResult {
    pub memory: Memory,
    pub score: f64,
    pub fts_rank: Option<usize>,
    pub vector_rank: Option<usize>,
    pub vector_similarity: Option<f32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemoryJobStatus {
    Pending,
    Complete,
    Failed,
}

impl MemoryJobStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Complete => "complete",
            Self::Failed => "failed",
        }
    }

    fn from_db(value: String) -> rusqlite::Result<Self> {
        match value.as_str() {
            "pending" => Ok(Self::Pending),
            "complete" => Ok(Self::Complete),
            "failed" => Ok(Self::Failed),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                format!("unknown memory job status {other:?}").into(),
            )),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewMemoryJob {
    pub memory_id: Option<i64>,
    pub source_message_id: Option<i64>,
    pub job_type: String,
    pub payload: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryJob {
    pub id: i64,
    pub memory_id: Option<i64>,
    pub job_type: String,
    pub payload: String,
    pub status: MemoryJobStatus,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub completed_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PurgeReport {
    pub checkpoint_busy: i64,
    pub checkpoint_log_pages: i64,
    pub checkpointed_pages: i64,
}

/// Creates or migrates all memory-owned tables, indexes, triggers, and FTS5 data.
///
/// This function is idempotent. It enables foreign keys for the supplied
/// connection because SQLite configures that setting per connection.
pub fn init_memory_schema(conn: &Connection) -> MemoryResult<()> {
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS memory_schema_migrations (
             version INTEGER PRIMARY KEY,
             applied_at INTEGER NOT NULL DEFAULT (unixepoch())
         );",
    )?;

    let found = memory_schema_version(conn)?;
    if found > MEMORY_SCHEMA_VERSION {
        return Err(MemoryError::UnsupportedSchema {
            found,
            supported: MEMORY_SCHEMA_VERSION,
        });
    }

    for version in (found + 1)..=MEMORY_SCHEMA_VERSION {
        let tx = conn.unchecked_transaction()?;
        apply_migration(&tx, version)?;
        tx.execute(
            "INSERT INTO memory_schema_migrations(version) VALUES (?1)",
            [version],
        )?;
        tx.commit()?;
    }

    Ok(())
}

pub fn memory_schema_version(conn: &Connection) -> MemoryResult<i64> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM sqlite_master
             WHERE type = 'table' AND name = 'memory_schema_migrations'
         )",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(0);
    }

    Ok(conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM memory_schema_migrations",
        [],
        |row| row.get(0),
    )?)
}

fn apply_migration(tx: &Transaction<'_>, version: i64) -> MemoryResult<()> {
    match version {
        1 => tx.execute_batch(
            r#"
            CREATE TABLE memories (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                text TEXT NOT NULL,
                normalized_text TEXT NOT NULL UNIQUE,
                kind TEXT NOT NULL,
                scope TEXT NOT NULL CHECK (scope IN ('global', 'repo')),
                repo TEXT,
                pinned INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)),
                importance REAL NOT NULL DEFAULT 0.5 CHECK (importance >= 0.0 AND importance <= 1.0),
                confidence REAL NOT NULL DEFAULT 1.0 CHECK (confidence >= 0.0 AND confidence <= 1.0),
                sensitivity TEXT NOT NULL DEFAULT 'normal'
                    CHECK (sensitivity IN ('normal', 'sensitive')),
                expires_at INTEGER,
                source_message_id INTEGER,
                superseded_by INTEGER REFERENCES memories(id) ON DELETE SET NULL,
                created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
                last_accessed_at INTEGER,
                access_count INTEGER NOT NULL DEFAULT 0 CHECK (access_count >= 0),
                CHECK (expires_at IS NULL OR expires_at > 0),
                CHECK (source_message_id IS NULL OR source_message_id > 0),
                CHECK (superseded_by IS NULL OR superseded_by != id),
                CHECK (
                    (scope = 'global' AND repo IS NULL) OR
                    (scope = 'repo' AND repo IS NOT NULL AND length(trim(repo)) > 0)
                )
            );

            CREATE INDEX memories_active_scope_repo_idx
                ON memories(superseded_by, expires_at, scope, repo, pinned DESC, updated_at DESC);

            CREATE INDEX memories_source_message_idx
                ON memories(source_message_id);

            CREATE TABLE memory_embeddings (
                memory_id INTEGER PRIMARY KEY
                    REFERENCES memories(id) ON DELETE CASCADE,
                model TEXT NOT NULL,
                dimensions INTEGER NOT NULL CHECK (dimensions > 0),
                vector BLOB NOT NULL,
                created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                updated_at INTEGER NOT NULL DEFAULT (unixepoch())
            );

            CREATE INDEX memory_embeddings_model_dimensions_idx
                ON memory_embeddings(model, dimensions);

            CREATE TABLE memory_jobs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                memory_id INTEGER REFERENCES memories(id) ON DELETE CASCADE,
                job_type TEXT NOT NULL,
                payload TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL DEFAULT 'pending'
                    CHECK (status IN ('pending', 'complete', 'failed')),
                attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
                last_error TEXT,
                created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
                completed_at INTEGER
            );

            CREATE INDEX memory_jobs_status_created_idx
                ON memory_jobs(status, created_at, id);

            CREATE VIRTUAL TABLE memory_fts USING fts5(
                text,
                kind,
                repo,
                content='memories',
                content_rowid='id'
            );

            CREATE TRIGGER memories_fts_insert AFTER INSERT ON memories BEGIN
                INSERT INTO memory_fts(rowid, text, kind, repo)
                VALUES (new.id, new.text, new.kind, new.repo);
            END;

            CREATE TRIGGER memories_fts_delete AFTER DELETE ON memories BEGIN
                INSERT INTO memory_fts(memory_fts, rowid, text, kind, repo)
                VALUES ('delete', old.id, old.text, old.kind, old.repo);
            END;

            CREATE TRIGGER memories_fts_update AFTER UPDATE OF text, kind, repo ON memories BEGIN
                INSERT INTO memory_fts(memory_fts, rowid, text, kind, repo)
                VALUES ('delete', old.id, old.text, old.kind, old.repo);
                INSERT INTO memory_fts(rowid, text, kind, repo)
                VALUES (new.id, new.text, new.kind, new.repo);
            END;

            INSERT INTO memory_fts(memory_fts) VALUES ('rebuild');
            "#,
        )?,
        2 => tx.execute_batch(
            r#"
            ALTER TABLE memory_jobs ADD COLUMN source_message_id INTEGER;
            CREATE INDEX memory_jobs_source_message_idx
                ON memory_jobs(source_message_id);
            "#,
        )?,
        _ => {
            return Err(MemoryError::UnsupportedSchema {
                found: version,
                supported: MEMORY_SCHEMA_VERSION,
            })
        }
    }
    Ok(())
}

/// Adds a memory. Text that differs only by Unicode lowercase and whitespace is
/// deduplicated and returns the existing ID with `inserted == false`.
pub fn add_memory(conn: &Connection, input: &NewMemory) -> MemoryResult<AddMemoryOutcome> {
    let validated = ValidatedMemory::new(input)?;
    if let Some(existing_id) = find_memory_by_normalized_text(conn, &validated.normalized_text)? {
        return Ok(AddMemoryOutcome {
            id: existing_id,
            inserted: false,
        });
    }

    let tx = conn.unchecked_transaction()?;
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO memories(
             text, normalized_text, kind, scope, repo, pinned, importance,
             confidence, sensitivity, expires_at, source_message_id
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            validated.text,
            validated.normalized_text,
            validated.kind,
            validated.scope.as_str(),
            validated.repo,
            validated.pinned,
            validated.importance,
            validated.confidence,
            validated.sensitivity.as_str(),
            validated.expires_at,
            validated.source_message_id,
        ],
    )?;

    if inserted == 0 {
        let existing_id = find_memory_by_normalized_text(&tx, &validated.normalized_text)?
            .ok_or_else(|| MemoryError::InvalidInput("deduplication race lost".to_string()))?;
        tx.commit()?;
        return Ok(AddMemoryOutcome {
            id: existing_id,
            inserted: false,
        });
    }

    let id = tx.last_insert_rowid();
    if let Some(embedding) = &validated.embedding {
        write_embedding(&tx, id, embedding)?;
    }
    tx.commit()?;

    Ok(AddMemoryOutcome { id, inserted: true })
}

pub fn get_memory(conn: &Connection, id: i64) -> MemoryResult<Option<Memory>> {
    let mut statement = conn.prepare(&format!("{} WHERE m.id = ?1", memory_select_sql()))?;
    Ok(statement.query_row([id], map_memory_row).optional()?)
}

pub fn list_memories(conn: &Connection, options: &ListOptions) -> MemoryResult<Vec<Memory>> {
    let limit = checked_limit(options.limit, DEFAULT_LIMIT)? as i64;
    let offset = i64::try_from(options.offset)
        .map_err(|_| MemoryError::InvalidInput("offset is too large".to_string()))?;
    let mut sql = memory_select_sql().to_string();
    let mut parameters: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    let mut conditions = Vec::new();

    if !options.include_inactive {
        conditions.push(active_memory_condition().to_string());
    }

    match &options.scope {
        ListScope::All => {}
        ListScope::Global => conditions.push("m.scope = 'global'".to_string()),
        ListScope::Repo(repo) => {
            let repo = validate_repo(repo)?;
            conditions.push("m.scope = 'repo' AND m.repo = ?1".to_string());
            parameters.push(Box::new(repo));
        }
        ListScope::RepoAndGlobal(repo) => {
            let repo = validate_repo(repo)?;
            conditions
                .push("(m.scope = 'global' OR (m.scope = 'repo' AND m.repo = ?1))".to_string());
            parameters.push(Box::new(repo));
        }
    }

    if !conditions.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conditions.join(" AND "));
    }

    let limit_index = parameters.len() + 1;
    let offset_index = parameters.len() + 2;
    sql.push_str(&format!(
        " ORDER BY m.pinned DESC, m.updated_at DESC, m.id DESC LIMIT ?{limit_index} OFFSET ?{offset_index}"
    ));
    parameters.push(Box::new(limit));
    parameters.push(Box::new(offset));

    let refs: Vec<&dyn rusqlite::ToSql> = parameters.iter().map(Box::as_ref).collect();
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map(refs.as_slice(), map_memory_row)?;
    collect_rows(rows)
}

/// Edits a memory and returns the new record.
///
/// Changing the text without supplying a replacement embedding removes the old
/// embedding so stale semantic data cannot be retrieved.
pub fn edit_memory(conn: &Connection, id: i64, update: &MemoryUpdate) -> MemoryResult<Memory> {
    let existing = get_memory(conn, id)?.ok_or(MemoryError::NotFound {
        entity: "memory",
        id,
    })?;

    let text = update.text.as_deref().unwrap_or(&existing.text).trim();
    if text.is_empty() {
        return Err(MemoryError::InvalidInput(
            "memory text cannot be empty".to_string(),
        ));
    }
    let normalized_text = normalize_text(text);
    if let Some(existing_id) = find_memory_by_normalized_text(conn, &normalized_text)? {
        if existing_id != id {
            return Err(MemoryError::DuplicateMemory { existing_id });
        }
    }

    let kind = update.kind.as_deref().unwrap_or(&existing.kind).trim();
    if kind.is_empty() {
        return Err(MemoryError::InvalidInput(
            "memory kind cannot be empty".to_string(),
        ));
    }
    let scope = update.scope.as_ref().unwrap_or(&existing.scope);
    let requested_repo = update
        .repo
        .as_ref()
        .cloned()
        .unwrap_or(existing.repo.clone());
    let repo = validate_scope_repo(scope, requested_repo)?;
    let importance = update.importance.unwrap_or(existing.importance);
    validate_importance(importance)?;
    let confidence = update.confidence.unwrap_or(existing.confidence);
    validate_confidence(confidence)?;
    let sensitivity = update.sensitivity.as_ref().unwrap_or(&existing.sensitivity);
    let expires_at = update.expires_at.unwrap_or(existing.expires_at);
    validate_optional_positive_id(expires_at, "expires_at")?;
    let source_message_id = update
        .source_message_id
        .unwrap_or(existing.source_message_id);
    validate_optional_positive_id(source_message_id, "source_message_id")?;
    let pinned = update.pinned.unwrap_or(existing.pinned);

    let replacement_embedding = match &update.embedding {
        EmbeddingUpdate::Replace(embedding) => Some(ValidatedEmbedding::new(embedding)?),
        _ => None,
    };

    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE memories SET
             text = ?1,
             normalized_text = ?2,
             kind = ?3,
             scope = ?4,
             repo = ?5,
             pinned = ?6,
             importance = ?7,
             confidence = ?8,
             sensitivity = ?9,
             expires_at = ?10,
             source_message_id = ?11,
             updated_at = unixepoch()
         WHERE id = ?12",
        params![
            text,
            normalized_text,
            kind,
            scope.as_str(),
            repo,
            pinned,
            importance,
            confidence,
            sensitivity.as_str(),
            expires_at,
            source_message_id,
            id,
        ],
    )?;

    let text_changed = normalized_text != existing.normalized_text;
    match (&update.embedding, replacement_embedding) {
        (EmbeddingUpdate::Replace(_), Some(embedding)) => write_embedding(&tx, id, &embedding)?,
        (EmbeddingUpdate::Remove, _) => {
            tx.execute("DELETE FROM memory_embeddings WHERE memory_id = ?1", [id])?;
        }
        (EmbeddingUpdate::Keep, _) if text_changed => {
            tx.execute("DELETE FROM memory_embeddings WHERE memory_id = ?1", [id])?;
        }
        _ => {}
    }
    tx.commit()?;

    get_memory(conn, id)?.ok_or(MemoryError::NotFound {
        entity: "memory",
        id,
    })
}

/// Marks an older memory as replaced by an active memory while preserving its
/// provenance for management and audit views.
pub fn supersede_memory(
    conn: &Connection,
    old_id: i64,
    replacement_id: i64,
) -> MemoryResult<Memory> {
    if old_id == replacement_id {
        return Err(MemoryError::InvalidInput(
            "a memory cannot supersede itself".to_string(),
        ));
    }
    get_memory(conn, old_id)?.ok_or(MemoryError::NotFound {
        entity: "memory",
        id: old_id,
    })?;
    let replacement = get_memory(conn, replacement_id)?.ok_or(MemoryError::NotFound {
        entity: "replacement memory",
        id: replacement_id,
    })?;
    if !is_active_memory(&replacement, unix_timestamp()) {
        return Err(MemoryError::InvalidInput(
            "replacement memory must be active".to_string(),
        ));
    }

    conn.execute(
        "UPDATE memories SET
             superseded_by = ?2,
             expires_at = CASE
                 WHEN expires_at IS NULL OR expires_at > unixepoch() THEN unixepoch()
                 ELSE expires_at
             END,
             updated_at = unixepoch()
         WHERE id = ?1",
        params![old_id, replacement_id],
    )?;

    get_memory(conn, old_id)?.ok_or(MemoryError::NotFound {
        entity: "memory",
        id: old_id,
    })
}

pub fn delete_memory(conn: &Connection, id: i64) -> MemoryResult<bool> {
    Ok(conn.execute("DELETE FROM memories WHERE id = ?1", [id])? > 0)
}

pub fn clear_memories(conn: &Connection, scope: &ClearScope) -> MemoryResult<usize> {
    let changed = match scope {
        ClearScope::All => {
            let tx = conn.unchecked_transaction()?;
            tx.execute("DELETE FROM memory_jobs", [])?;
            tx.execute("DELETE FROM memory_embeddings", [])?;
            let changed = tx.execute("DELETE FROM memories", [])?;
            tx.commit()?;
            changed
        }
        ClearScope::Global => conn.execute("DELETE FROM memories WHERE scope = 'global'", [])?,
        ClearScope::Repo(repo) => conn.execute(
            "DELETE FROM memories WHERE scope = 'repo' AND repo = ?1",
            [validate_repo(repo)?],
        )?,
    };
    Ok(changed)
}

/// Performs an exact full scan over stored embeddings matching both the query
/// model and dimensions. Vectors are normalized before storage, making the dot
/// product an exact cosine similarity for the persisted representation.
pub fn exact_vector_search(
    conn: &Connection,
    embedding: &Embedding,
    repo: Option<&str>,
    include_global: bool,
    limit: usize,
) -> MemoryResult<Vec<VectorMatch>> {
    let limit = checked_limit(limit, 10)?;
    let query = ValidatedEmbedding::new(embedding)?;
    let repo = repo.map(validate_repo).transpose()?;
    let mut statement = conn.prepare(&format!(
        "{}, e.vector
         FROM memories m
         JOIN memory_embeddings e ON e.memory_id = m.id
         WHERE e.model = ?1
           AND e.dimensions = ?2
           AND m.superseded_by IS NULL
           AND (m.expires_at IS NULL OR m.expires_at > unixepoch())
           AND (
               (?3 IS NULL AND m.scope = 'global') OR
               (?3 IS NOT NULL AND (
                   (?4 = 1 AND m.scope = 'global') OR
                   (m.scope = 'repo' AND m.repo = ?3)
               ))
           )",
        memory_select_columns()
    ))?;

    let rows = statement.query_map(
        params![query.model, query.dimensions as i64, repo, include_global,],
        |row| {
            let memory = map_memory_row(row)?;
            let blob: Vec<u8> = row.get(19)?;
            Ok((memory, blob))
        },
    )?;

    let mut matches = Vec::new();
    for row in rows {
        let (memory, blob) = row?;
        let vector = decode_vector(memory.id, &blob, query.dimensions)?;
        let similarity = dot_product(&query.normalized, &vector);
        matches.push(VectorMatch { memory, similarity });
    }

    matches.sort_by(|left, right| {
        right
            .similarity
            .partial_cmp(&left.similarity)
            .unwrap_or(Ordering::Equal)
            .then_with(|| right.memory.pinned.cmp(&left.memory.pinned))
            .then_with(|| right.memory.updated_at.cmp(&left.memory.updated_at))
            .then_with(|| right.memory.id.cmp(&left.memory.id))
    });
    matches.truncate(limit);
    Ok(matches)
}

/// Searches with FTS5 and/or an exact vector scan, combines source ranks with
/// reciprocal-rank fusion, then applies modest repo, pin, importance, and
/// recency boosts.
pub fn search_memories(
    conn: &Connection,
    query: &MemoryQuery,
) -> MemoryResult<Vec<MemorySearchResult>> {
    let limit = checked_limit(query.limit, 10)?;
    let candidate_limit = checked_limit(query.candidate_limit, DEFAULT_CANDIDATE_LIMIT)?.max(limit);
    let repo = query.repo.as_deref().map(validate_repo).transpose()?;
    let mut candidates: HashMap<i64, HybridCandidate> = HashMap::new();

    if let Some(fts_query) = build_fts_query(&query.text) {
        for (rank, memory) in fts_candidates(
            conn,
            &fts_query,
            repo.as_deref(),
            query.include_global,
            candidate_limit,
        )?
        .into_iter()
        .enumerate()
        {
            candidates
                .entry(memory.id)
                .or_insert_with(|| HybridCandidate::new(memory))
                .fts_rank = Some(rank + 1);
        }
    }

    if let Some(embedding) = &query.embedding {
        for (rank, vector_match) in exact_vector_search(
            conn,
            embedding,
            repo.as_deref(),
            query.include_global,
            candidate_limit,
        )?
        .into_iter()
        .enumerate()
        {
            let candidate = candidates
                .entry(vector_match.memory.id)
                .or_insert_with(|| HybridCandidate::new(vector_match.memory));
            candidate.vector_rank = Some(rank + 1);
            candidate.vector_similarity = Some(vector_match.similarity);
        }
    }

    let now = unix_timestamp();
    let mut results: Vec<MemorySearchResult> = candidates
        .into_values()
        .map(|candidate| candidate.finish(repo.as_deref(), now))
        .collect();
    results.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| right.memory.pinned.cmp(&left.memory.pinned))
            .then_with(|| right.memory.updated_at.cmp(&left.memory.updated_at))
            .then_with(|| right.memory.id.cmp(&left.memory.id))
    });
    results.truncate(limit);
    Ok(results)
}

/// Records that a retrieved memory was actually used by the caller.
pub fn touch_memory(conn: &Connection, id: i64) -> MemoryResult<bool> {
    Ok(conn.execute(
        "UPDATE memories SET
             last_accessed_at = unixepoch(),
             access_count = access_count + 1
         WHERE id = ?1
           AND superseded_by IS NULL
           AND (expires_at IS NULL OR expires_at > unixepoch())",
        [id],
    )? > 0)
}

pub fn queue_memory_job(conn: &Connection, input: &NewMemoryJob) -> MemoryResult<i64> {
    let job_type = input.job_type.trim();
    if job_type.is_empty() {
        return Err(MemoryError::InvalidInput(
            "memory job type cannot be empty".to_string(),
        ));
    }
    conn.execute(
        "INSERT INTO memory_jobs(memory_id, source_message_id, job_type, payload)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            input.memory_id,
            input.source_message_id,
            job_type,
            input.payload
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn list_memory_jobs(
    conn: &Connection,
    status: MemoryJobStatus,
    limit: usize,
) -> MemoryResult<Vec<MemoryJob>> {
    let limit = checked_limit(limit, DEFAULT_LIMIT)? as i64;
    let mut statement = conn.prepare(
        "SELECT id, memory_id, job_type, payload, status, attempts, last_error,
                created_at, updated_at, completed_at
         FROM memory_jobs
         WHERE status = ?1
         ORDER BY created_at, id
         LIMIT ?2",
    )?;
    let rows = statement.query_map(params![status.as_str(), limit], map_job_row)?;
    collect_rows(rows)
}

/// Return the next extraction job whose bounded retry delay has elapsed.
pub fn next_retryable_memory_job(
    conn: &Connection,
    excluded_id: i64,
) -> MemoryResult<Option<MemoryJob>> {
    Ok(conn
        .query_row(
            "SELECT id, memory_id, job_type, payload, status, attempts, last_error,
                    created_at, updated_at, completed_at
             FROM memory_jobs
             WHERE status = 'pending'
               AND id != ?1
               AND attempts < ?2
               AND (
                    attempts = 0 OR
                    updated_at <= unixepoch() - CASE attempts
                        WHEN 1 THEN 60
                        WHEN 2 THEN 300
                        ELSE 3600
                    END
               )
             ORDER BY created_at, id
             LIMIT 1",
            params![excluded_id, MAX_MEMORY_JOB_ATTEMPTS],
            map_job_row,
        )
        .optional()?)
}

pub fn complete_memory_job(conn: &Connection, id: i64) -> MemoryResult<bool> {
    Ok(conn.execute(
        "UPDATE memory_jobs SET
             status = 'complete',
             payload = '',
             last_error = NULL,
             updated_at = unixepoch(),
             completed_at = unixepoch()
         WHERE id = ?1 AND status = 'pending'",
        [id],
    )? > 0)
}

pub fn fail_memory_job(conn: &Connection, id: i64, error: impl AsRef<str>) -> MemoryResult<bool> {
    let error = error.as_ref().trim();
    if error.is_empty() {
        return Err(MemoryError::InvalidInput(
            "memory job failure must include an error".to_string(),
        ));
    }
    Ok(conn.execute(
        "UPDATE memory_jobs SET
             status = 'failed',
             payload = '',
             attempts = attempts + 1,
             last_error = ?2,
             updated_at = unixepoch(),
             completed_at = NULL
         WHERE id = ?1 AND status = 'pending'",
        params![id, error],
    )? > 0)
}

/// Record a transient extraction failure. Jobs retry after 1 and 5 minutes,
/// then stop permanently after the third failed attempt and discard payloads.
pub fn retry_memory_job(conn: &Connection, id: i64, error: impl AsRef<str>) -> MemoryResult<bool> {
    let error = error.as_ref().trim();
    if error.is_empty() {
        return Err(MemoryError::InvalidInput(
            "memory job retry must include an error".to_string(),
        ));
    }
    Ok(conn.execute(
        "UPDATE memory_jobs SET
             status = CASE WHEN attempts + 1 >= ?3 THEN 'failed' ELSE 'pending' END,
             payload = CASE WHEN attempts + 1 >= ?3 THEN '' ELSE payload END,
             attempts = attempts + 1,
             last_error = ?2,
             updated_at = unixepoch(),
             completed_at = CASE WHEN attempts + 1 >= ?3 THEN unixepoch() ELSE NULL END
         WHERE id = ?1 AND status = 'pending'",
        params![id, error, MAX_MEMORY_JOB_ATTEMPTS],
    )? > 0)
}

/// Remove queued chat-extraction payloads before their source messages are cleared.
pub fn delete_memory_jobs_for_chat(conn: &Connection, chat_id: i64) -> MemoryResult<usize> {
    Ok(conn.execute(
        "DELETE FROM memory_jobs
         WHERE source_message_id IN (SELECT id FROM messages WHERE chat_id = ?1)",
        [chat_id],
    )?)
}

/// Remove all queued or historical jobs tied to chat messages before chats are cleared.
pub fn delete_all_chat_memory_jobs(conn: &Connection) -> MemoryResult<usize> {
    Ok(conn.execute(
        "DELETE FROM memory_jobs WHERE source_message_id IS NOT NULL",
        [],
    )?)
}

pub fn get_memory_job(conn: &Connection, id: i64) -> MemoryResult<Option<MemoryJob>> {
    Ok(conn
        .query_row(
            "SELECT id, memory_id, job_type, payload, status, attempts, last_error,
                    created_at, updated_at, completed_at
             FROM memory_jobs
             WHERE id = ?1",
            [id],
            map_job_row,
        )
        .optional()?)
}

/// Enables SQLite's strongest ordinary-table overwrite behavior for subsequent
/// updates and deletes on this connection.
pub fn enable_secure_delete(conn: &Connection) -> MemoryResult<()> {
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA secure_delete = ON;",
    )?;
    let enabled: i64 = conn.query_row("PRAGMA secure_delete", [], |row| row.get(0))?;
    if enabled != 1 {
        return Err(MemoryError::InvalidInput(
            "SQLite did not enable secure_delete".to_string(),
        ));
    }
    Ok(())
}

/// Compacts FTS5, truncates any WAL, and VACUUMs the database.
///
/// Call only when no transaction is active. `VACUUM` is intentionally explicit
/// because it can be expensive and take an exclusive database lock.
pub fn purge_deleted_content(conn: &Connection) -> MemoryResult<PurgeReport> {
    conn.execute("INSERT INTO memory_fts(memory_fts) VALUES ('optimize')", [])?;
    let (checkpoint_busy, checkpoint_log_pages, checkpointed_pages) =
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    conn.execute_batch("VACUUM;")?;
    Ok(PurgeReport {
        checkpoint_busy,
        checkpoint_log_pages,
        checkpointed_pages,
    })
}

/// Securely clears all memory-owned content and compacts the database.
pub fn secure_clear_all_memory(conn: &Connection) -> MemoryResult<PurgeReport> {
    enable_secure_delete(conn)?;
    clear_memories(conn, &ClearScope::All)?;
    purge_deleted_content(conn)
}

fn memory_select_sql() -> &'static str {
    "SELECT m.id, m.text, m.normalized_text, m.kind, m.scope, m.repo,
            m.pinned, m.importance, m.confidence, m.sensitivity, m.expires_at,
            m.source_message_id, m.superseded_by, m.created_at, m.updated_at,
            m.last_accessed_at, m.access_count, e.model, e.dimensions
     FROM memories m
     LEFT JOIN memory_embeddings e ON e.memory_id = m.id"
}

fn memory_select_columns() -> &'static str {
    "SELECT m.id, m.text, m.normalized_text, m.kind, m.scope, m.repo,
            m.pinned, m.importance, m.confidence, m.sensitivity, m.expires_at,
            m.source_message_id, m.superseded_by, m.created_at, m.updated_at,
            m.last_accessed_at, m.access_count, e.model, e.dimensions"
}

fn map_memory_row(row: &Row<'_>) -> rusqlite::Result<Memory> {
    let dimensions: Option<i64> = row.get(18)?;
    let access_count: i64 = row.get(16)?;
    Ok(Memory {
        id: row.get(0)?,
        text: row.get(1)?,
        normalized_text: row.get(2)?,
        kind: row.get(3)?,
        scope: MemoryScope::from_db(row.get(4)?)?,
        repo: row.get(5)?,
        pinned: row.get(6)?,
        importance: row.get(7)?,
        confidence: row.get(8)?,
        sensitivity: MemorySensitivity::from_db(row.get(9)?)?,
        expires_at: row.get(10)?,
        source_message_id: row.get(11)?,
        superseded_by: row.get(12)?,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
        last_accessed_at: row.get(15)?,
        access_count: u64::try_from(access_count).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                16,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        embedding_model: row.get(17)?,
        embedding_dimensions: dimensions
            .map(|value| {
                usize::try_from(value).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        18,
                        rusqlite::types::Type::Integer,
                        Box::new(error),
                    )
                })
            })
            .transpose()?,
    })
}

fn map_job_row(row: &Row<'_>) -> rusqlite::Result<MemoryJob> {
    let attempts: i64 = row.get(5)?;
    Ok(MemoryJob {
        id: row.get(0)?,
        memory_id: row.get(1)?,
        job_type: row.get(2)?,
        payload: row.get(3)?,
        status: MemoryJobStatus::from_db(row.get(4)?)?,
        attempts: u32::try_from(attempts).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                5,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        last_error: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
        completed_at: row.get(9)?,
    })
}

fn collect_rows<T, F>(rows: rusqlite::MappedRows<'_, F>) -> MemoryResult<Vec<T>>
where
    F: FnMut(&Row<'_>) -> rusqlite::Result<T>,
{
    let mut values = Vec::new();
    for row in rows {
        values.push(row?);
    }
    Ok(values)
}

fn find_memory_by_normalized_text(
    conn: &Connection,
    normalized_text: &str,
) -> MemoryResult<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT id FROM memories WHERE normalized_text = ?1",
            [normalized_text],
            |row| row.get(0),
        )
        .optional()?)
}

fn write_embedding(
    conn: &Connection,
    memory_id: i64,
    embedding: &ValidatedEmbedding,
) -> MemoryResult<()> {
    let blob = encode_vector(&embedding.normalized);
    conn.execute(
        "INSERT INTO memory_embeddings(memory_id, model, dimensions, vector)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(memory_id) DO UPDATE SET
             model = excluded.model,
             dimensions = excluded.dimensions,
             vector = excluded.vector,
             updated_at = unixepoch()",
        params![
            memory_id,
            embedding.model,
            embedding.dimensions as i64,
            blob,
        ],
    )?;
    Ok(())
}

fn fts_candidates(
    conn: &Connection,
    fts_query: &str,
    repo: Option<&str>,
    include_global: bool,
    limit: usize,
) -> MemoryResult<Vec<Memory>> {
    let mut statement = conn.prepare(&format!(
        "{}
         JOIN memory_fts ON memory_fts.rowid = m.id
         WHERE memory_fts MATCH ?1
           AND m.superseded_by IS NULL
           AND (m.expires_at IS NULL OR m.expires_at > unixepoch())
           AND (
               (?2 IS NULL AND m.scope = 'global') OR
               (?2 IS NOT NULL AND (
                   (?3 = 1 AND m.scope = 'global') OR
                   (m.scope = 'repo' AND m.repo = ?2)
               ))
           )
         ORDER BY bm25(memory_fts), m.pinned DESC, m.updated_at DESC, m.id DESC
         LIMIT ?4",
        memory_select_sql()
    ))?;
    let rows = statement.query_map(
        params![fts_query, repo, include_global, limit as i64],
        map_memory_row,
    )?;
    collect_rows(rows)
}

fn build_fts_query(text: &str) -> Option<String> {
    const STOP_WORDS: &[&str] = &[
        "a", "an", "and", "are", "for", "how", "i", "in", "is", "it", "of", "on", "the", "this",
        "to", "was", "what", "why",
    ];
    let mut terms = Vec::new();
    let mut current = String::new();
    for character in text.chars().chain(std::iter::once(' ')) {
        if character.is_alphanumeric() || character == '_' {
            current.extend(character.to_lowercase());
        } else if !current.is_empty() {
            if !STOP_WORDS.contains(&current.as_str()) && !terms.contains(&current) {
                terms.push(std::mem::take(&mut current));
                if terms.len() == 16 {
                    break;
                }
            } else {
                current.clear();
            }
        }
    }
    if terms.is_empty() {
        None
    } else {
        Some(
            terms
                .into_iter()
                .map(|term| format!("\"{term}\""))
                .collect::<Vec<_>>()
                .join(" OR "),
        )
    }
}

fn normalize_text(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn validate_repo(repo: impl AsRef<str>) -> MemoryResult<String> {
    let repo = repo.as_ref().trim();
    if repo.is_empty() {
        return Err(MemoryError::InvalidInput(
            "repo scope requires a non-empty repo".to_string(),
        ));
    }
    Ok(repo.to_string())
}

fn validate_scope_repo(scope: &MemoryScope, repo: Option<String>) -> MemoryResult<Option<String>> {
    match scope {
        MemoryScope::Global => Ok(None),
        MemoryScope::Repo => repo.map(validate_repo).transpose()?.map_or_else(
            || {
                Err(MemoryError::InvalidInput(
                    "repo scope requires a repo".to_string(),
                ))
            },
            |repo| Ok(Some(repo)),
        ),
    }
}

fn validate_importance(importance: f64) -> MemoryResult<()> {
    if !importance.is_finite() || !(0.0..=1.0).contains(&importance) {
        return Err(MemoryError::InvalidInput(
            "importance must be a finite value from 0.0 through 1.0".to_string(),
        ));
    }
    Ok(())
}

fn validate_confidence(confidence: f64) -> MemoryResult<()> {
    if !confidence.is_finite() || !(0.0..=1.0).contains(&confidence) {
        return Err(MemoryError::InvalidInput(
            "confidence must be a finite value from 0.0 through 1.0".to_string(),
        ));
    }
    Ok(())
}

fn validate_optional_positive_id(value: Option<i64>, field: &str) -> MemoryResult<()> {
    if value.is_some_and(|value| value <= 0) {
        return Err(MemoryError::InvalidInput(format!(
            "{field} must be a positive integer when present"
        )));
    }
    Ok(())
}

fn active_memory_condition() -> &'static str {
    "m.superseded_by IS NULL AND (m.expires_at IS NULL OR m.expires_at > unixepoch())"
}

fn is_active_memory(memory: &Memory, now: i64) -> bool {
    memory.superseded_by.is_none() && memory.expires_at.is_none_or(|expires_at| expires_at > now)
}

fn checked_limit(limit: usize, default: usize) -> MemoryResult<usize> {
    let limit = if limit == 0 { default } else { limit };
    if limit > MAX_QUERY_LIMIT {
        return Err(MemoryError::InvalidInput(format!(
            "limit cannot exceed {MAX_QUERY_LIMIT}"
        )));
    }
    Ok(limit)
}

fn encode_vector(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(std::mem::size_of_val(vector));
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn decode_vector(memory_id: i64, blob: &[u8], dimensions: usize) -> MemoryResult<Vec<f32>> {
    let expected_bytes = dimensions
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or_else(|| MemoryError::CorruptEmbedding {
            memory_id,
            reason: "dimension byte count overflowed".to_string(),
        })?;
    if blob.len() != expected_bytes {
        return Err(MemoryError::CorruptEmbedding {
            memory_id,
            reason: format!(
                "expected {expected_bytes} bytes for {dimensions} dimensions, found {}",
                blob.len()
            ),
        });
    }

    let mut vector = Vec::with_capacity(dimensions);
    for chunk in blob.chunks_exact(4) {
        let value = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        if !value.is_finite() {
            return Err(MemoryError::CorruptEmbedding {
                memory_id,
                reason: "vector contains a non-finite value".to_string(),
            });
        }
        vector.push(value);
    }

    let norm = vector
        .iter()
        .map(|value| (*value as f64) * (*value as f64))
        .sum::<f64>()
        .sqrt();
    if norm <= f64::EPSILON {
        return Err(MemoryError::CorruptEmbedding {
            memory_id,
            reason: "vector has zero magnitude".to_string(),
        });
    }
    for value in &mut vector {
        *value = (*value as f64 / norm) as f32;
    }
    Ok(vector)
}

fn dot_product(left: &[f32], right: &[f32]) -> f32 {
    left.iter()
        .zip(right)
        .map(|(left, right)| left * right)
        .sum()
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

struct ValidatedMemory {
    text: String,
    normalized_text: String,
    kind: String,
    scope: MemoryScope,
    repo: Option<String>,
    pinned: bool,
    importance: f64,
    confidence: f64,
    sensitivity: MemorySensitivity,
    expires_at: Option<i64>,
    source_message_id: Option<i64>,
    embedding: Option<ValidatedEmbedding>,
}

impl ValidatedMemory {
    fn new(input: &NewMemory) -> MemoryResult<Self> {
        let text = input.text.trim();
        if text.is_empty() {
            return Err(MemoryError::InvalidInput(
                "memory text cannot be empty".to_string(),
            ));
        }
        let kind = input.kind.trim();
        if kind.is_empty() {
            return Err(MemoryError::InvalidInput(
                "memory kind cannot be empty".to_string(),
            ));
        }
        validate_importance(input.importance)?;
        validate_confidence(input.confidence)?;
        validate_optional_positive_id(input.expires_at, "expires_at")?;
        validate_optional_positive_id(input.source_message_id, "source_message_id")?;

        Ok(Self {
            text: text.to_string(),
            normalized_text: normalize_text(text),
            kind: kind.to_string(),
            scope: input.scope.clone(),
            repo: validate_scope_repo(&input.scope, input.repo.clone())?,
            pinned: input.pinned,
            importance: input.importance,
            confidence: input.confidence,
            sensitivity: input.sensitivity.clone(),
            expires_at: input.expires_at,
            source_message_id: input.source_message_id,
            embedding: input
                .embedding
                .as_ref()
                .map(ValidatedEmbedding::new)
                .transpose()?,
        })
    }
}

struct ValidatedEmbedding {
    model: String,
    dimensions: usize,
    normalized: Vec<f32>,
}

impl ValidatedEmbedding {
    fn new(embedding: &Embedding) -> MemoryResult<Self> {
        let model = embedding.model.trim();
        if model.is_empty() {
            return Err(MemoryError::InvalidInput(
                "embedding model cannot be empty".to_string(),
            ));
        }
        if embedding.vector.is_empty() {
            return Err(MemoryError::InvalidInput(
                "embedding vector cannot be empty".to_string(),
            ));
        }
        if embedding.vector.iter().any(|value| !value.is_finite()) {
            return Err(MemoryError::InvalidInput(
                "embedding vector must contain only finite values".to_string(),
            ));
        }
        let norm = embedding
            .vector
            .iter()
            .map(|value| (*value as f64) * (*value as f64))
            .sum::<f64>()
            .sqrt();
        if norm <= f64::EPSILON {
            return Err(MemoryError::InvalidInput(
                "embedding vector must have non-zero magnitude".to_string(),
            ));
        }
        let normalized = embedding
            .vector
            .iter()
            .map(|value| (*value as f64 / norm) as f32)
            .collect();
        Ok(Self {
            model: model.to_string(),
            dimensions: embedding.vector.len(),
            normalized,
        })
    }
}

struct HybridCandidate {
    memory: Memory,
    fts_rank: Option<usize>,
    vector_rank: Option<usize>,
    vector_similarity: Option<f32>,
}

impl HybridCandidate {
    fn new(memory: Memory) -> Self {
        Self {
            memory,
            fts_rank: None,
            vector_rank: None,
            vector_similarity: None,
        }
    }

    fn finish(self, query_repo: Option<&str>, now: i64) -> MemorySearchResult {
        let mut score = 0.0;
        if let Some(rank) = self.fts_rank {
            score += 1.0 / (RRF_K + rank as f64);
        }
        if let Some(rank) = self.vector_rank {
            score += 1.0 / (RRF_K + rank as f64);
        }

        if self.memory.pinned {
            score *= 1.15;
        }
        if query_repo.is_some() && self.memory.repo.as_deref() == query_repo {
            score *= 1.08;
        }
        score *= 0.9 + 0.2 * self.memory.importance;
        score *= 0.85 + 0.15 * self.memory.confidence;

        let age = now.saturating_sub(self.memory.updated_at);
        if age <= WEEK_SECONDS {
            score *= 1.10;
        } else if age <= MONTH_SECONDS {
            score *= 1.05;
        }

        MemorySearchResult {
            memory: self.memory,
            score,
            fts_rank: self.fts_rank,
            vector_rank: self.vector_rank,
            vector_similarity: self.vector_similarity,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory database");
        init_memory_schema(&conn).expect("initialize memory schema");
        conn
    }

    fn embedded(text: &str, model: &str, vector: &[f32]) -> NewMemory {
        let mut memory = NewMemory::global(text);
        memory.embedding = Some(Embedding::new(model, vector.to_vec()));
        memory
    }

    #[test]
    fn schema_initialization_is_versioned_and_idempotent() {
        let conn = database();
        init_memory_schema(&conn).expect("initialize twice");
        assert_eq!(memory_schema_version(&conn).unwrap(), MEMORY_SCHEMA_VERSION);

        for object in [
            "memories",
            "memory_embeddings",
            "memory_jobs",
            "memory_fts",
            "memories_active_scope_repo_idx",
            "memories_source_message_idx",
            "memories_fts_insert",
            "memories_fts_update",
            "memories_fts_delete",
            "memory_jobs_source_message_idx",
        ] {
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = ?1)",
                    [object],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(exists, "expected schema object {object}");
        }
    }

    #[test]
    fn add_deduplicates_normalized_text_and_fts_tracks_edits_and_deletes() {
        let conn = database();
        let first = add_memory(&conn, &NewMemory::global("  Use   nvim config  ")).unwrap();
        let duplicate = add_memory(&conn, &NewMemory::global("use nvim CONFIG")).unwrap();
        assert!(first.inserted);
        assert!(!duplicate.inserted);
        assert_eq!(first.id, duplicate.id);

        let found = search_memories(&conn, &MemoryQuery::text("nvim")).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].memory.id, first.id);

        let updated = edit_memory(
            &conn,
            first.id,
            &MemoryUpdate {
                text: Some("Use helix config".to_string()),
                ..MemoryUpdate::default()
            },
        )
        .unwrap();
        assert_eq!(updated.text, "Use helix config");
        assert!(search_memories(&conn, &MemoryQuery::text("nvim"))
            .unwrap()
            .is_empty());
        assert_eq!(
            search_memories(&conn, &MemoryQuery::text("helix")).unwrap()[0]
                .memory
                .id,
            first.id
        );

        assert!(delete_memory(&conn, first.id).unwrap());
        assert!(!delete_memory(&conn, first.id).unwrap());
        assert!(search_memories(&conn, &MemoryQuery::text("helix"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn list_edit_and_clear_honor_scope_and_embedding_freshness() {
        let conn = database();
        let global = add_memory(&conn, &embedded("global command", "tiny", &[1.0, 0.0]))
            .unwrap()
            .id;
        let mut repo_memory = NewMemory::repo("repo command", "/work/yo");
        repo_memory.embedding = Some(Embedding::new("tiny", vec![0.0, 1.0]));
        let repo = add_memory(&conn, &repo_memory).unwrap().id;

        let scoped = list_memories(
            &conn,
            &ListOptions {
                scope: ListScope::RepoAndGlobal("/work/yo".to_string()),
                ..ListOptions::default()
            },
        )
        .unwrap();
        assert_eq!(scoped.len(), 2);

        let changed = edit_memory(
            &conn,
            global,
            &MemoryUpdate {
                text: Some("changed global command".to_string()),
                ..MemoryUpdate::default()
            },
        )
        .unwrap();
        assert_eq!(changed.embedding_model, None);

        assert_eq!(
            clear_memories(&conn, &ClearScope::Repo("/work/yo".into())).unwrap(),
            1
        );
        assert!(get_memory(&conn, repo).unwrap().is_none());
        assert!(get_memory(&conn, global).unwrap().is_some());
        assert_eq!(clear_memories(&conn, &ClearScope::All).unwrap(), 1);
        assert!(list_memories(&conn, &ListOptions::default())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn exact_vector_search_normalizes_and_filters_model_dimension_and_scope() {
        let conn = database();
        let axis = add_memory(&conn, &embedded("x axis", "tiny", &[2.0, 0.0]))
            .unwrap()
            .id;
        add_memory(&conn, &embedded("y axis", "tiny", &[0.0, 3.0])).unwrap();
        add_memory(&conn, &embedded("other model", "other", &[1.0, 0.0])).unwrap();
        add_memory(
            &conn,
            &embedded("other dimensions", "tiny", &[1.0, 0.0, 0.0]),
        )
        .unwrap();
        let mut other_repo = NewMemory::repo("private repo", "/other");
        other_repo.embedding = Some(Embedding::new("tiny", vec![1.0, 0.0]));
        add_memory(&conn, &other_repo).unwrap();

        let matches = exact_vector_search(
            &conn,
            &Embedding::new("tiny", vec![10.0, 0.0]),
            None,
            true,
            10,
        )
        .unwrap();
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].memory.id, axis);
        assert!((matches[0].similarity - 1.0).abs() < 0.0001);

        assert!(exact_vector_search(
            &conn,
            &Embedding::new("missing", vec![1.0, 0.0]),
            None,
            true,
            10,
        )
        .unwrap()
        .is_empty());
        assert!(exact_vector_search(
            &conn,
            &Embedding::new("tiny", vec![0.0, 0.0]),
            None,
            true,
            10,
        )
        .is_err());
    }

    #[test]
    fn hybrid_search_combines_ranks_and_enforces_repo_global_visibility() {
        let conn = database();
        let mut global = embedded("open nvim configuration", "tiny", &[1.0, 0.0]);
        global.pinned = true;
        let global_id = add_memory(&conn, &global).unwrap().id;

        let mut repo = NewMemory::repo("nvim project settings", "/work/yo");
        repo.embedding = Some(Embedding::new("tiny", vec![0.9, 0.1]));
        let repo_id = add_memory(&conn, &repo).unwrap().id;

        let mut hidden = NewMemory::repo("nvim secret settings", "/work/other");
        hidden.embedding = Some(Embedding::new("tiny", vec![1.0, 0.0]));
        let hidden_id = add_memory(&conn, &hidden).unwrap().id;

        let results = search_memories(
            &conn,
            &MemoryQuery {
                text: "nvim settings".to_string(),
                embedding: Some(Embedding::new("tiny", vec![1.0, 0.0])),
                repo: Some("/work/yo".to_string()),
                include_global: true,
                limit: 10,
                candidate_limit: 10,
            },
        )
        .unwrap();
        let ids: Vec<i64> = results.iter().map(|result| result.memory.id).collect();
        assert!(ids.contains(&global_id));
        assert!(ids.contains(&repo_id));
        assert!(!ids.contains(&hidden_id));
        assert!(results.iter().all(|result| result.score > 0.0));
        assert!(results
            .iter()
            .any(|result| result.fts_rank.is_some() && result.vector_rank.is_some()));

        let global_only = search_memories(
            &conn,
            &MemoryQuery {
                text: "nvim".to_string(),
                embedding: None,
                repo: None,
                include_global: true,
                limit: 10,
                candidate_limit: 10,
            },
        )
        .unwrap();
        assert_eq!(global_only.len(), 1);
        assert_eq!(global_only[0].memory.id, global_id);
    }

    #[test]
    fn jobs_transition_once_and_record_failures() {
        let conn = database();
        let memory_id = add_memory(&conn, &NewMemory::global("job target"))
            .unwrap()
            .id;
        let complete_id = queue_memory_job(
            &conn,
            &NewMemoryJob {
                memory_id: Some(memory_id),
                source_message_id: None,
                job_type: "embed".to_string(),
                payload: "{}".to_string(),
            },
        )
        .unwrap();
        let failed_id = queue_memory_job(
            &conn,
            &NewMemoryJob {
                memory_id: None,
                source_message_id: None,
                job_type: "extract".to_string(),
                payload: "turn:1".to_string(),
            },
        )
        .unwrap();

        assert_eq!(
            list_memory_jobs(&conn, MemoryJobStatus::Pending, 10)
                .unwrap()
                .len(),
            2
        );
        assert!(complete_memory_job(&conn, complete_id).unwrap());
        assert!(!complete_memory_job(&conn, complete_id).unwrap());
        assert!(fail_memory_job(&conn, failed_id, "offline").unwrap());

        let complete = get_memory_job(&conn, complete_id).unwrap().unwrap();
        assert!(complete.payload.is_empty());

        let failed = get_memory_job(&conn, failed_id).unwrap().unwrap();
        assert_eq!(failed.status, MemoryJobStatus::Failed);
        assert_eq!(failed.attempts, 1);
        assert_eq!(failed.last_error.as_deref(), Some("offline"));
        assert!(failed.payload.is_empty());
    }

    #[test]
    fn transient_jobs_back_off_and_stop_after_three_failures() {
        let conn = database();
        let id = queue_memory_job(
            &conn,
            &NewMemoryJob {
                memory_id: None,
                source_message_id: None,
                job_type: "extract".to_string(),
                payload: "private turn".to_string(),
            },
        )
        .unwrap();

        assert_eq!(
            next_retryable_memory_job(&conn, -1).unwrap().unwrap().id,
            id
        );
        assert!(retry_memory_job(&conn, id, "offline").unwrap());
        assert!(next_retryable_memory_job(&conn, -1).unwrap().is_none());

        conn.execute(
            "UPDATE memory_jobs SET updated_at = unixepoch() - 301 WHERE id = ?1",
            [id],
        )
        .unwrap();
        assert_eq!(
            next_retryable_memory_job(&conn, -1).unwrap().unwrap().id,
            id
        );
        assert!(retry_memory_job(&conn, id, "offline again").unwrap());

        conn.execute(
            "UPDATE memory_jobs SET updated_at = unixepoch() - 301 WHERE id = ?1",
            [id],
        )
        .unwrap();
        assert!(retry_memory_job(&conn, id, "still offline").unwrap());
        let job = get_memory_job(&conn, id).unwrap().unwrap();
        assert_eq!(job.status, MemoryJobStatus::Failed);
        assert_eq!(job.attempts, 3);
        assert!(job.payload.is_empty());
        assert!(next_retryable_memory_job(&conn, -1).unwrap().is_none());
    }

    #[test]
    fn clearing_a_chat_removes_its_pending_extraction_payloads() {
        let conn = database();
        conn.execute_batch(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, chat_id INTEGER NOT NULL);
             INSERT INTO messages(id, chat_id) VALUES (10, 1), (20, 2);",
        )
        .unwrap();
        for (source_message_id, payload) in [(10, "first private turn"), (20, "other turn")] {
            queue_memory_job(
                &conn,
                &NewMemoryJob {
                    memory_id: None,
                    source_message_id: Some(source_message_id),
                    job_type: "extract-turn".into(),
                    payload: payload.into(),
                },
            )
            .unwrap();
        }

        assert_eq!(delete_memory_jobs_for_chat(&conn, 1).unwrap(), 1);
        let remaining = list_memory_jobs(&conn, MemoryJobStatus::Pending, 10).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].payload, "other turn");
    }

    #[test]
    fn duplicate_edits_are_rejected_and_touch_updates_usage() {
        let conn = database();
        let first = add_memory(&conn, &NewMemory::global("first memory"))
            .unwrap()
            .id;
        let second = add_memory(&conn, &NewMemory::global("second memory"))
            .unwrap()
            .id;

        let error = edit_memory(
            &conn,
            second,
            &MemoryUpdate {
                text: Some(" FIRST   MEMORY ".to_string()),
                ..MemoryUpdate::default()
            },
        )
        .unwrap_err();
        assert!(matches!(
            error,
            MemoryError::DuplicateMemory { existing_id } if existing_id == first
        ));

        assert!(touch_memory(&conn, first).unwrap());
        let memory = get_memory(&conn, first).unwrap().unwrap();
        assert_eq!(memory.access_count, 1);
        assert!(memory.last_accessed_at.is_some());
    }

    #[test]
    fn durable_fields_round_trip_and_reject_invalid_values() {
        let conn = database();
        let mut input = NewMemory::global("private editor preference");
        input.confidence = 0.7;
        input.sensitivity = MemorySensitivity::Sensitive;
        input.expires_at = Some(unix_timestamp() + 3_600);
        input.source_message_id = Some(42);

        let id = add_memory(&conn, &input).unwrap().id;
        let memory = get_memory(&conn, id).unwrap().unwrap();
        assert_eq!(memory.confidence, 0.7);
        assert_eq!(memory.sensitivity, MemorySensitivity::Sensitive);
        assert_eq!(memory.expires_at, input.expires_at);
        assert_eq!(memory.source_message_id, Some(42));
        assert_eq!(memory.superseded_by, None);

        let updated = edit_memory(
            &conn,
            id,
            &MemoryUpdate {
                confidence: Some(0.9),
                sensitivity: Some(MemorySensitivity::Normal),
                expires_at: Some(None),
                source_message_id: Some(None),
                ..MemoryUpdate::default()
            },
        )
        .unwrap();
        assert_eq!(updated.confidence, 0.9);
        assert_eq!(updated.sensitivity, MemorySensitivity::Normal);
        assert_eq!(updated.expires_at, None);
        assert_eq!(updated.source_message_id, None);

        let mut invalid_confidence = NewMemory::global("invalid confidence");
        invalid_confidence.confidence = f64::NAN;
        assert!(matches!(
            add_memory(&conn, &invalid_confidence),
            Err(MemoryError::InvalidInput(_))
        ));

        let mut invalid_source = NewMemory::global("invalid source");
        invalid_source.source_message_id = Some(0);
        assert!(matches!(
            add_memory(&conn, &invalid_source),
            Err(MemoryError::InvalidInput(_))
        ));
    }

    #[test]
    fn expired_and_superseded_memories_are_inactive_by_default() {
        let conn = database();
        let mut expired = embedded("old nvim command", "tiny", &[1.0, 0.0]);
        expired.expires_at = Some(unix_timestamp().saturating_sub(1));
        let expired_id = add_memory(&conn, &expired).unwrap().id;
        let old_id = add_memory(&conn, &embedded("legacy nvim mapping", "tiny", &[1.0, 0.0]))
            .unwrap()
            .id;
        let replacement_id = add_memory(
            &conn,
            &embedded("current nvim mapping", "tiny", &[1.0, 0.0]),
        )
        .unwrap()
        .id;

        let old = supersede_memory(&conn, old_id, replacement_id).unwrap();
        assert_eq!(old.superseded_by, Some(replacement_id));
        assert!(!touch_memory(&conn, old_id).unwrap());

        let active = list_memories(&conn, &ListOptions::default()).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, replacement_id);

        let all = list_memories(
            &conn,
            &ListOptions {
                include_inactive: true,
                ..ListOptions::default()
            },
        )
        .unwrap();
        assert_eq!(all.len(), 3);
        assert!(all.iter().any(|memory| memory.id == expired_id));
        assert!(all.iter().any(|memory| memory.id == old_id));

        let lexical = search_memories(&conn, &MemoryQuery::text("nvim mapping")).unwrap();
        assert_eq!(lexical.len(), 1);
        assert_eq!(lexical[0].memory.id, replacement_id);

        let semantic = exact_vector_search(
            &conn,
            &Embedding::new("tiny", vec![1.0, 0.0]),
            None,
            true,
            10,
        )
        .unwrap();
        assert_eq!(semantic.len(), 1);
        assert_eq!(semantic[0].memory.id, replacement_id);
    }

    #[test]
    fn superseding_requires_an_active_distinct_replacement() {
        let conn = database();
        let old_id = add_memory(&conn, &NewMemory::global("old fact"))
            .unwrap()
            .id;
        let mut expired = NewMemory::global("expired fact");
        expired.expires_at = Some(unix_timestamp().saturating_sub(1));
        let expired_id = add_memory(&conn, &expired).unwrap().id;

        assert!(matches!(
            supersede_memory(&conn, old_id, old_id),
            Err(MemoryError::InvalidInput(_))
        ));
        assert!(matches!(
            supersede_memory(&conn, old_id, expired_id),
            Err(MemoryError::InvalidInput(_))
        ));
        assert!(matches!(
            supersede_memory(&conn, old_id, 99_999),
            Err(MemoryError::NotFound { .. })
        ));
    }

    #[test]
    fn secure_clear_removes_all_memory_owned_rows() {
        let conn = database();
        let id = add_memory(&conn, &embedded("erase me", "tiny", &[1.0, 0.0]))
            .unwrap()
            .id;
        queue_memory_job(
            &conn,
            &NewMemoryJob {
                memory_id: Some(id),
                source_message_id: None,
                job_type: "embed".into(),
                payload: "sensitive".into(),
            },
        )
        .unwrap();

        secure_clear_all_memory(&conn).unwrap();
        for table in ["memories", "memory_embeddings", "memory_jobs"] {
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "expected {table} to be empty");
        }
    }
}
