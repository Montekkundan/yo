use crate::config::get_app_dir;
use rusqlite::{params, Connection, DatabaseName, OpenFlags, OptionalExtension, Result};
use std::fs;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatMessage {
    pub id: i64,
    pub role: String,
    pub content: String,
    pub cwd: Option<String>,
    pub created_at: String,
}

#[derive(Clone, Debug)]
pub struct ChatInfo {
    pub id: i64,
    pub title: String,
    pub session_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug)]
pub struct TerminalEvent {
    pub id: i64,
    pub command: String,
    pub exit_code: i32,
    pub cwd: String,
    pub stdout: String,
    pub stderr: String,
    pub created_at: String,
}

pub struct NewTerminalEvent<'a> {
    pub session_id: &'a str,
    pub command: &'a str,
    pub exit_code: i32,
    pub cwd: &'a str,
    pub stdout: &'a str,
    pub stderr: &'a str,
    pub duration_ms: u128,
}

pub fn get_db_path() -> PathBuf {
    get_app_dir().join("chats.db")
}

pub fn default_backup_path() -> PathBuf {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    get_app_dir()
        .join("backups")
        .join(format!("yo-{timestamp}.db"))
}

pub fn backup_database(output: Option<&Path>) -> anyhow::Result<PathBuf> {
    let uses_default_directory = output.is_none();
    let destination = output
        .map(Path::to_path_buf)
        .unwrap_or_else(default_backup_path);
    if destination.exists() {
        anyhow::bail!("backup already exists: {}", destination.display());
    }
    if let Some(parent) = destination.parent() {
        let parent_existed = parent.is_dir();
        fs::create_dir_all(parent)?;
        if uses_default_directory || !parent_existed {
            set_private_directory_permissions(parent)?;
        }
    }
    let conn = init_db()?;
    conn.execute_batch("PRAGMA wal_checkpoint(FULL);")?;
    create_private_backup_file(&destination)?;
    if let Err(error) = conn.backup(DatabaseName::Main, &destination, None) {
        let _ = fs::remove_file(&destination);
        return Err(error.into());
    }
    set_private_backup_permissions(&destination)?;
    Ok(destination)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepairReport {
    pub backup: PathBuf,
    pub integrity_before: String,
    pub integrity_after: String,
}

pub fn integrity_check() -> anyhow::Result<String> {
    let conn = init_db()?;
    integrity_check_connection(&conn)
}

pub fn integrity_check_existing() -> anyhow::Result<String> {
    let path = get_db_path();
    if !path.is_file() {
        anyhow::bail!("database does not exist; run `yo setup`");
    }
    // FTS5's integrity validation may update its internal validation state even
    // though this helper does not run migrations or change user data.
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    integrity_check_connection(&conn)
}

pub fn repair_database() -> anyhow::Result<RepairReport> {
    let backup = backup_database(None)?;
    let conn = init_db()?;
    let integrity_before = integrity_check_connection(&conn)?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); REINDEX; PRAGMA optimize; VACUUM;")?;
    let integrity_after = integrity_check_connection(&conn)?;
    if integrity_after != "ok" {
        anyhow::bail!(
            "database still fails integrity checks after repair; untouched backup: {} ({})",
            backup.display(),
            integrity_after
        );
    }
    Ok(RepairReport {
        backup,
        integrity_before,
        integrity_after,
    })
}

fn integrity_check_connection(conn: &Connection) -> anyhow::Result<String> {
    Ok(conn.query_row("PRAGMA integrity_check;", [], |row| row.get(0))?)
}

pub fn init_db() -> Result<Connection> {
    let path = get_db_path();
    backup_database_before_migration(&path)?;
    let conn = Connection::open(&path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    initialize_schema(&conn)?;
    set_private_permissions(&path);
    Ok(conn)
}

pub(crate) fn initialize_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        PRAGMA foreign_keys = ON;
        PRAGMA journal_mode = WAL;
        PRAGMA secure_delete = ON;

        CREATE TABLE IF NOT EXISTS schema_versions (
            component TEXT PRIMARY KEY,
            version INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            shell TEXT,
            tty TEXT,
            initial_cwd TEXT,
            repo TEXT,
            active_chat_id INTEGER,
            rolling_summary TEXT,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            updated_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY(active_chat_id) REFERENCES chats(id) ON DELETE SET NULL
        );

        CREATE TABLE IF NOT EXISTS chats (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            title TEXT NOT NULL DEFAULT 'New Chat',
            session_id TEXT,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            updated_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            system_prompt TEXT,
            summary TEXT,
            summary_up_to_message_id INTEGER,
            tags TEXT,
            FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE SET NULL
        );

        CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            chat_id INTEGER NOT NULL,
            role TEXT NOT NULL,
            content TEXT NOT NULL,
            cwd TEXT,
            terminal_event_id INTEGER,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY(chat_id) REFERENCES chats(id) ON DELETE CASCADE,
            FOREIGN KEY(terminal_event_id) REFERENCES terminal_events(id) ON DELETE SET NULL
        );

        CREATE TABLE IF NOT EXISTS terminal_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            command TEXT NOT NULL,
            exit_code INTEGER NOT NULL,
            cwd TEXT NOT NULL,
            stdout TEXT NOT NULL DEFAULT '',
            stderr TEXT NOT NULL DEFAULT '',
            duration_ms INTEGER NOT NULL DEFAULT 0,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS user_profile (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        "#,
    )?;

    migrate_legacy_columns(conn)?;
    conn.execute_batch(
        r#"
        CREATE INDEX IF NOT EXISTS idx_chats_session ON chats(session_id, updated_at DESC);
        CREATE INDEX IF NOT EXISTS idx_messages_chat ON messages(chat_id, id);
        CREATE INDEX IF NOT EXISTS idx_terminal_events_session ON terminal_events(session_id, id DESC);
        "#,
    )?;
    conn.execute(
        "INSERT INTO schema_versions(component, version) VALUES('core', 3) ON CONFLICT(component) DO UPDATE SET version=excluded.version",
        [],
    )?;
    Ok(())
}

fn backup_database_before_migration(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let backup = path.with_extension("pre-gateway.bak");
    if backup.exists() {
        return Ok(());
    }

    let probe = Connection::open(path)?;
    let current_version = probe
        .query_row(
            "SELECT version FROM schema_versions WHERE component='core'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional();
    if matches!(current_version, Ok(Some(version)) if version >= 3) {
        return Ok(());
    }
    let _ = probe.execute_batch("PRAGMA wal_checkpoint(FULL);");
    drop(probe);
    fs::copy(path, &backup)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    set_private_permissions(&backup);
    Ok(())
}

fn migrate_legacy_columns(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "chats", "session_id", "TEXT")?;
    add_column_if_missing(conn, "chats", "summary", "TEXT")?;
    add_column_if_missing(conn, "chats", "summary_up_to_message_id", "INTEGER")?;
    add_column_if_missing(conn, "messages", "cwd", "TEXT")?;
    add_column_if_missing(conn, "messages", "terminal_event_id", "INTEGER")?;
    Ok(())
}

fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<()> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let exists = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .flatten()
        .any(|name| name == column);
    if !exists {
        conn.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {definition};"
        ))?;
    }
    Ok(())
}

pub fn ensure_session(
    conn: &Connection,
    session_id: &str,
    shell: &str,
    tty: Option<&str>,
    cwd: &str,
    repo: Option<&str>,
) -> Result<i64> {
    conn.execute(
        r#"INSERT INTO sessions(id, shell, tty, initial_cwd, repo)
           VALUES(?1, ?2, ?3, ?4, ?5)
           ON CONFLICT(id) DO UPDATE SET
             shell=excluded.shell,
             tty=COALESCE(excluded.tty, sessions.tty),
             repo=COALESCE(excluded.repo, sessions.repo),
             updated_at=CURRENT_TIMESTAMP"#,
        params![session_id, shell, tty, cwd, repo],
    )?;

    if let Some(chat_id) = conn
        .query_row(
            "SELECT active_chat_id FROM sessions WHERE id=?1",
            [session_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten()
    {
        return Ok(chat_id);
    }
    create_chat(conn, session_id, "New terminal chat")
}

pub fn create_chat(conn: &Connection, session_id: &str, title: &str) -> Result<i64> {
    conn.execute(
        "INSERT INTO chats(title, session_id) VALUES(?1, ?2)",
        params![title, session_id],
    )?;
    let id = conn.last_insert_rowid();
    conn.execute(
        "UPDATE sessions SET active_chat_id=?1, updated_at=CURRENT_TIMESTAMP WHERE id=?2",
        params![id, session_id],
    )?;
    Ok(id)
}

pub fn active_chat_id(conn: &Connection, session_id: &str) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT active_chat_id FROM sessions WHERE id=?1",
        [session_id],
        |row| row.get(0),
    )
    .optional()
    .map(Option::flatten)
}

pub fn switch_chat(conn: &Connection, session_id: &str, chat_id: i64) -> Result<bool> {
    let exists = conn
        .query_row("SELECT 1 FROM chats WHERE id=?1", [chat_id], |_| Ok(()))
        .optional()?
        .is_some();
    if exists {
        conn.execute(
            "UPDATE sessions SET active_chat_id=?1, updated_at=CURRENT_TIMESTAMP WHERE id=?2",
            params![chat_id, session_id],
        )?;
    }
    Ok(exists)
}

pub fn insert_message(
    conn: &Connection,
    chat_id: i64,
    role: &str,
    content: &str,
    cwd: Option<&str>,
    terminal_event_id: Option<i64>,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO messages(chat_id, role, content, cwd, terminal_event_id) VALUES(?1, ?2, ?3, ?4, ?5)",
        params![chat_id, role, content, cwd, terminal_event_id],
    )?;
    conn.execute(
        "UPDATE chats SET updated_at=CURRENT_TIMESTAMP WHERE id=?1",
        [chat_id],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn recent_messages(conn: &Connection, chat_id: i64, limit: usize) -> Result<Vec<ChatMessage>> {
    let mut statement = conn.prepare(
        r#"SELECT id, role, content, cwd, created_at FROM (
             SELECT id, role, content, cwd, created_at
             FROM messages WHERE chat_id=?1 ORDER BY id DESC LIMIT ?2
           ) ORDER BY id ASC"#,
    )?;
    let rows = statement
        .query_map(params![chat_id, limit as i64], |row| {
            Ok(ChatMessage {
                id: row.get(0)?,
                role: row.get(1)?,
                content: row.get(2)?,
                cwd: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?
        .collect();
    rows
}

pub fn chat_summary(conn: &Connection, chat_id: i64) -> Result<Option<String>> {
    conn.query_row("SELECT summary FROM chats WHERE id=?1", [chat_id], |row| {
        row.get(0)
    })
    .optional()
    .map(Option::flatten)
}

pub fn messages_to_compact(
    conn: &Connection,
    chat_id: i64,
    retain_newest: usize,
    limit: usize,
) -> Result<Vec<ChatMessage>> {
    let retain_newest = i64::try_from(retain_newest).unwrap_or(i64::MAX);
    let cutoff = conn
        .query_row(
            "SELECT id FROM messages WHERE chat_id=?1 ORDER BY id DESC LIMIT 1 OFFSET ?2",
            params![chat_id, retain_newest],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    let Some(cutoff) = cutoff else {
        return Ok(Vec::new());
    };
    let summary_up_to = conn
        .query_row(
            "SELECT summary_up_to_message_id FROM chats WHERE id=?1",
            [chat_id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten()
        .unwrap_or(0);
    let mut statement = conn.prepare(
        "SELECT id, role, content, cwd, created_at FROM messages \
         WHERE chat_id=?1 AND id>?2 AND id<=?3 ORDER BY id ASC LIMIT ?4",
    )?;
    let rows = statement
        .query_map(
            params![
                chat_id,
                summary_up_to,
                cutoff,
                i64::try_from(limit).unwrap_or(i64::MAX)
            ],
            |row| {
                Ok(ChatMessage {
                    id: row.get(0)?,
                    role: row.get(1)?,
                    content: row.get(2)?,
                    cwd: row.get(3)?,
                    created_at: row.get(4)?,
                })
            },
        )?
        .collect();
    rows
}

pub fn update_chat_summary(
    conn: &Connection,
    chat_id: i64,
    summary: &str,
    up_to_message_id: i64,
) -> Result<()> {
    conn.execute(
        "UPDATE chats SET summary=?2, summary_up_to_message_id=?3 WHERE id=?1",
        params![chat_id, summary, up_to_message_id],
    )?;
    Ok(())
}

pub fn list_chats(conn: &Connection) -> Result<Vec<ChatInfo>> {
    let mut statement = conn.prepare(
        "SELECT id, title, session_id, created_at, updated_at FROM chats ORDER BY updated_at DESC, id DESC",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok(ChatInfo {
                id: row.get(0)?,
                title: row.get(1)?,
                session_id: row.get(2)?,
                created_at: row.get(3)?,
                updated_at: row.get(4)?,
            })
        })?
        .collect();
    rows
}

pub fn search_messages(conn: &Connection, query: &str) -> Result<Vec<(i64, String, String)>> {
    let pattern = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
    let mut statement = conn.prepare(
        "SELECT chat_id, role, content FROM messages WHERE content LIKE ?1 ESCAPE '\\' ORDER BY id DESC LIMIT 100",
    )?;
    let rows = statement
        .query_map([pattern], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect();
    rows
}

pub fn delete_chat(conn: &Connection, chat_id: i64) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM messages WHERE chat_id=?1", [chat_id])?;
    let deleted = tx.execute("DELETE FROM chats WHERE id=?1", [chat_id])?;
    tx.commit()?;
    Ok(deleted)
}

pub fn clear_chat(conn: &Connection, chat_id: i64) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    let deleted = tx.execute("DELETE FROM messages WHERE chat_id=?1", [chat_id])?;
    tx.execute(
        "UPDATE chats SET summary=NULL, summary_up_to_message_id=NULL, updated_at=CURRENT_TIMESTAMP WHERE id=?1",
        [chat_id],
    )?;
    tx.commit()?;
    Ok(deleted)
}

pub fn clear_all_chats(conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("UPDATE sessions SET active_chat_id=NULL", [])?;
    tx.execute("DELETE FROM messages", [])?;
    tx.execute("DELETE FROM chats", [])?;
    tx.commit()?;
    Ok(())
}

pub fn insert_terminal_event(conn: &Connection, event: &NewTerminalEvent<'_>) -> Result<i64> {
    conn.execute(
        r#"INSERT INTO terminal_events(
             session_id, command, exit_code, cwd, stdout, stderr, duration_ms
           ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)"#,
        params![
            event.session_id,
            event.command,
            event.exit_code,
            event.cwd,
            event.stdout,
            event.stderr,
            event.duration_ms.min(i64::MAX as u128) as i64
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn last_terminal_event(conn: &Connection, session_id: &str) -> Result<Option<TerminalEvent>> {
    conn.query_row(
        r#"SELECT id, command, exit_code, cwd, stdout, stderr, created_at
           FROM terminal_events WHERE session_id=?1 ORDER BY id DESC LIMIT 1"#,
        [session_id],
        |row| {
            Ok(TerminalEvent {
                id: row.get(0)?,
                command: row.get(1)?,
                exit_code: row.get(2)?,
                cwd: row.get(3)?,
                stdout: row.get(4)?,
                stderr: row.get(5)?,
                created_at: row.get(6)?,
            })
        },
    )
    .optional()
}

pub fn purge_database(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")
}

fn set_private_permissions(_path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(_path, fs::Permissions::from_mode(0o600));
    }
}

fn create_private_backup_file(path: &Path) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map(drop)
}

fn set_private_backup_permissions(_path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(_path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn set_private_directory_permissions(_path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(_path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            PRAGMA foreign_keys=ON;
            CREATE TABLE chats (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL DEFAULT 'New Chat', session_id TEXT, created_at TEXT DEFAULT CURRENT_TIMESTAMP, updated_at TEXT DEFAULT CURRENT_TIMESTAMP, system_prompt TEXT, summary TEXT, summary_up_to_message_id INTEGER, tags TEXT);
            CREATE TABLE sessions (id TEXT PRIMARY KEY, shell TEXT, tty TEXT, initial_cwd TEXT, repo TEXT, active_chat_id INTEGER, rolling_summary TEXT, created_at TEXT DEFAULT CURRENT_TIMESTAMP, updated_at TEXT DEFAULT CURRENT_TIMESTAMP, FOREIGN KEY(active_chat_id) REFERENCES chats(id) ON DELETE SET NULL);
            CREATE TABLE terminal_events (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, command TEXT NOT NULL, exit_code INTEGER NOT NULL, cwd TEXT NOT NULL, stdout TEXT NOT NULL DEFAULT '', stderr TEXT NOT NULL DEFAULT '', duration_ms INTEGER NOT NULL DEFAULT 0, created_at TEXT DEFAULT CURRENT_TIMESTAMP, FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE);
            CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, chat_id INTEGER NOT NULL, role TEXT NOT NULL, content TEXT NOT NULL, cwd TEXT, terminal_event_id INTEGER, created_at TEXT DEFAULT CURRENT_TIMESTAMP, FOREIGN KEY(chat_id) REFERENCES chats(id) ON DELETE CASCADE, FOREIGN KEY(terminal_event_id) REFERENCES terminal_events(id) ON DELETE SET NULL);
            "#,
        )
        .unwrap();
        conn
    }

    #[test]
    fn same_session_reuses_chat_and_new_session_gets_another() {
        let conn = connection();
        let first = ensure_session(&conn, "one", "zsh", None, "/tmp", None).unwrap();
        let again = ensure_session(&conn, "one", "zsh", None, "/tmp", None).unwrap();
        let second = ensure_session(&conn, "two", "zsh", None, "/tmp", None).unwrap();
        assert_eq!(first, again);
        assert_ne!(first, second);
    }

    #[test]
    fn legacy_schema_migrates_before_new_indexes_are_created() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE chats (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                title TEXT NOT NULL DEFAULT 'New Chat',
                created_at TEXT DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                chat_id INTEGER NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at TEXT DEFAULT CURRENT_TIMESTAMP
            );
            INSERT INTO chats(title) VALUES ('legacy');
            INSERT INTO messages(chat_id, role, content) VALUES (1, 'user', 'kept');
            "#,
        )
        .unwrap();

        initialize_schema(&conn).unwrap();

        let has_session_id: bool = conn
            .prepare("PRAGMA table_info(chats)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .flatten()
            .any(|name| name == "session_id");
        assert!(has_session_id);
        assert!(delete_chat(&conn, 1).unwrap() > 0);
        let messages: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(messages, 0);
    }

    #[test]
    fn recent_messages_are_chronological() {
        let conn = connection();
        let chat = ensure_session(&conn, "one", "zsh", None, "/tmp", None).unwrap();
        insert_message(&conn, chat, "user", "first", None, None).unwrap();
        insert_message(&conn, chat, "assistant", "second", None, None).unwrap();
        let messages = recent_messages(&conn, chat, 10).unwrap();
        assert_eq!(messages[0].content, "first");
        assert_eq!(messages[1].content, "second");
    }

    #[test]
    fn compaction_selects_only_turns_older_than_the_recent_window() {
        let conn = connection();
        let chat = ensure_session(&conn, "one", "zsh", None, "/tmp", None).unwrap();
        for index in 1..=6 {
            insert_message(
                &conn,
                chat,
                if index % 2 == 0 { "assistant" } else { "user" },
                &format!("message {index}"),
                None,
                None,
            )
            .unwrap();
        }

        let first = messages_to_compact(&conn, chat, 2, 100).unwrap();
        assert_eq!(
            first
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>(),
            ["message 1", "message 2", "message 3", "message 4"]
        );
        update_chat_summary(&conn, chat, "summary", first[2].id).unwrap();
        assert_eq!(
            chat_summary(&conn, chat).unwrap().as_deref(),
            Some("summary")
        );
        let remaining = messages_to_compact(&conn, chat, 2, 100).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].content, "message 4");
    }

    #[test]
    fn clear_chat_removes_messages_and_compacted_summary() {
        let conn = connection();
        let chat = ensure_session(&conn, "one", "zsh", None, "/tmp", None).unwrap();
        let message = insert_message(&conn, chat, "user", "private turn", None, None).unwrap();
        update_chat_summary(&conn, chat, "private summary", message).unwrap();

        assert_eq!(clear_chat(&conn, chat).unwrap(), 1);
        assert!(recent_messages(&conn, chat, 10).unwrap().is_empty());
        assert_eq!(chat_summary(&conn, chat).unwrap(), None);
    }
}
