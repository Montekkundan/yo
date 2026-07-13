use rusqlite::{Connection, Result};
use std::path::PathBuf;
use std::fs;

pub fn get_db_path() -> PathBuf {
    let config_dir = dirs::home_dir().unwrap().join(".config").join("yo");
    if !config_dir.exists() {
        let _ = fs::create_dir_all(&config_dir);
    }
    config_dir.join("chats.db")
}

pub fn init_db() -> Result<Connection> {
    let db_path = get_db_path();
    let conn = Connection::open(db_path)?;
    // Create tables if they don't exist
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS chats (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            title TEXT,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            updated_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            system_prompt TEXT,
            tags TEXT
        );
        CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            chat_id INTEGER,
            role TEXT,
            content TEXT,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY(chat_id) REFERENCES chats(id)
        );
        CREATE TABLE IF NOT EXISTS user_profile (
            key TEXT PRIMARY KEY,
            value TEXT
        );
        "#
    )?;
    Ok(conn)
} 