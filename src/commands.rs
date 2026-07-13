use crate::config::{get_config_path, load_or_create_config, save_config, Config};
use prettytable::{Table, Row, Cell};
use reqwest::Client;
use serde_json::Value;
use std::io::{self, Write};
use std::process::Command as ShellCommand;
use std::fs;
use async_trait::async_trait;
use anyhow::Result;
use futures_util::StreamExt;
use crate::db::init_db;

#[async_trait]
pub trait AIProvider {
    async fn ask_openai(&self, _messages: Vec<serde_json::Value>) -> Result<String> {
        Err(anyhow::anyhow!("Not implemented"))
    }
    async fn ask_ollama(&self, _prompt: &str) -> Result<String> {
        Err(anyhow::anyhow!("Not implemented"))
    }
}

pub struct OpenAIProvider {
    pub model: String,
    pub api_key: String,
}

#[async_trait]
impl AIProvider for OpenAIProvider {
    async fn ask_openai(&self, messages: Vec<serde_json::Value>) -> Result<String> {
        let client = reqwest::Client::new();
        let body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "stream": true
        });
        let res = client
            .post("https://api.openai.com/v1/chat/completions")
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;
        let status = res.status();
        let mut full = String::new();
        if !status.is_success() {
            let err_text = res.text().await.unwrap_or_default();
            eprintln!("OpenAI API error: {}\n{}", status, err_text);
            return Ok(String::new());
        }
        let mut stream = res.bytes_stream();
        let mut got_content = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            for line in chunk.split(|&b| b == b'\n') {
                if line.starts_with(b"data: ") {
                    let json = &line[6..];
                    if json == b"[DONE]" { continue; }
                    if let Ok(val) = serde_json::from_slice::<serde_json::Value>(json) {
                        if let Some(content) = val["choices"][0]["delta"]["content"].as_str() {
                            print!("{}", content);
                            std::io::stdout().flush().ok();
                            full.push_str(content);
                            got_content = true;
                        }
                    }
                } else if !line.is_empty() {
                    // Silently ignore non-content lines (do not print raw OpenAI stream lines)
                }
            }
        }
        if !got_content {
            eprintln!("No response from OpenAI. Check your API key, model, or network.");
        }
        println!();
        Ok(full)
    }
}

pub struct OllamaProvider {
    pub model: String,
}

#[async_trait]
impl AIProvider for OllamaProvider {
    async fn ask_ollama(&self, prompt: &str) -> Result<String> {
        use std::process::Command;
        let output = Command::new("ollama")
            .arg("run")
            .arg(&self.model)
            .arg(prompt)
            .output()?;
        let response = String::from_utf8_lossy(&output.stdout).to_string();
        Ok(response)
    }
}

pub enum Provider {
    OpenAI(OpenAIProvider),
    Ollama(OllamaProvider),
}

#[async_trait]
impl AIProvider for Provider {
    async fn ask_openai(&self, messages: Vec<serde_json::Value>) -> Result<String> {
        match self {
            Provider::OpenAI(p) => p.ask_openai(messages).await,
            _ => Err(anyhow::anyhow!("Not OpenAI provider")),
        }
    }
    async fn ask_ollama(&self, prompt: &str) -> Result<String> {
        match self {
            Provider::Ollama(p) => p.ask_ollama(prompt).await,
            _ => Err(anyhow::anyhow!("Not Ollama provider")),
        }
    }
}

async fn fetch_openai_models(api_key: &str) -> Vec<String> {
    let client = Client::new();
    let res = client
        .get("https://api.openai.com/v1/models")
        .bearer_auth(api_key)
        .send()
        .await
        .expect("failed to fetch models");
    let j: Value = res.json().await.expect("invalid JSON");
    j["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect()
}

fn fetch_ollama_local() -> Vec<String> {
    let out = ShellCommand::new("ollama").arg("list").output().expect("ollama list failed");
    let lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
        
    if lines.len() > 1 {
        lines.iter()
            .skip(1)
            .filter_map(|line| {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if !parts.is_empty() {
                    Some(parts[0].to_string())
                } else {
                    None
                }
            })
            .collect()
    } else {
        Vec::new()
    }
}

/// Clear the conversation history
pub fn clear_history() {
    let chat_id = match get_current_chat_id() {
        Some(id) => id,
        None => { eprintln!("No current chat selected. Start or switch to a chat first."); return; }
    };
    let conn = match init_db() {
        Ok(c) => c,
        Err(e) => { eprintln!("DB error: {}", e); return; }
    };
    if let Err(e) = conn.execute("DELETE FROM messages WHERE chat_id = ?1", [chat_id]) {
        eprintln!("Failed to clear history: {}", e);
    } else {
        println!("✅ Cleared history for current chat");
    }
}

/// Setup initial source & API key
pub fn setup() {
    // TODO: add more options
    print!("Choose backend (1) ollama  (2) openai: ");
    io::stdout().flush().unwrap();
    let mut c = String::new(); io::stdin().read_line(&mut c).unwrap();
    let src = match c.trim() {
        "1" => "ollama",
        "2" => "openai",
        _ => { eprintln!("invalid"); return; }
    }.to_string();

    let mut key = None;
    if src == "openai" {
        print!("Enter OpenAI API key: ");
        io::stdout().flush().unwrap();
        let mut k2 = String::new(); io::stdin().read_line(&mut k2).unwrap();
        key = Some(k2.trim().to_string());
    }

    // pick default model
    let default_model = if src == "openai" { "gpt-4".to_string() } else { 
        let loc = fetch_ollama_local();
        loc.get(0).cloned().unwrap_or_else(|| {
            eprintln!("no local ollama model installed");
            std::process::exit(1);
        })
    };

    let cfg = Config { source: src.clone(), model: default_model, openai_api_key: key };
    save_config(&cfg);
    println!("✅ setup complete");
    println!("⚙️ config saved at {}", get_config_path().display());
}

/// Show config path
pub fn show_config_path() {
    println!("{}", get_config_path().display());
}

/// Set a specific GPT model
pub async fn set_gpt(gpt_model: &str) {
    let mut cfg = load_or_create_config();
    cfg.source = "openai".into();
    
    // Make sure we have an API key
    if cfg.openai_api_key.is_none() {
        print!("Enter OpenAI API key: ");
        io::stdout().flush().unwrap();
        let mut k = String::new(); io::stdin().read_line(&mut k).unwrap();
        cfg.openai_api_key = Some(k.trim().to_string());
    }
    
    // Verify the model exists
    let models = fetch_openai_models(cfg.openai_api_key.as_ref().unwrap()).await;
    if models.iter().any(|m| m == gpt_model) {
        cfg.model = gpt_model.to_string();
        save_config(&cfg);
        println!("Switched to OpenAI model: {}", gpt_model);
        println!("⚙️ config saved at {}", get_config_path().display());
    } else {
        println!("Model '{}' not found in available OpenAI models.", gpt_model);
        println!("Available models:");
        for model in models.iter().take(10) {
            println!("  {}", model);
        }
        if models.len() > 10 {
            println!("  ... and {} more", models.len() - 10);
            println!("Run 'yo list' to see all available models");
        }
    }
}

/// Switch source or model
pub async fn switch(model: &str) {
    let mut cfg = load_or_create_config();
    if model == "openai" {
        cfg.source = "openai".into();
        if cfg.openai_api_key.is_none() {
            print!("Enter OpenAI API key: ");
            io::stdout().flush().unwrap();
            let mut k = String::new(); io::stdin().read_line(&mut k).unwrap();
            cfg.openai_api_key = Some(k.trim().to_string());
        }
        
        // Always use the previously selected OpenAI model if available in config
        // or default to gpt-4o if no OpenAI model was previously selected
        if !cfg.model.starts_with("gpt-") && 
           !["o1", "o3", "o4", "dall-e"].iter().any(|prefix| cfg.model.starts_with(prefix)) {
            // No OpenAI model found, set default
            cfg.model = "gpt-4o".to_string();
        }
        
        println!("Switched to OpenAI model: {}", cfg.model);
        save_config(&cfg);
        println!("⚙️ config saved at {}", get_config_path().display());
        return;
    } else if model == "ollama" {
        cfg.source = "ollama".into();
        let loc = fetch_ollama_local();
        if loc.is_empty() {
            eprintln!("❌ No local Ollama models found. Please install one first with:");
            eprintln!(" ollama pull llama3");
            eprintln!("\nVisit https://ollama.com/search to discover available models.");
            return;
        }
        
        // Check if the current model is an Ollama model (not starting with "gpt-" or other OpenAI prefixes)
        if !cfg.model.starts_with("gpt-") && 
           !["o1", "o3", "o4", "dall-e"].iter().any(|prefix| cfg.model.starts_with(prefix)) {
            // Current model is likely an Ollama model
            // Check if it exists in available models
            if loc.iter().any(|m| m == &cfg.model) {
                println!("Using previously selected Ollama model: {}", cfg.model);
                save_config(&cfg);
                println!("⚙️ config saved at {}", get_config_path().display());
                return;
            }
        }
        
        // If we don't have a valid Ollama model in config, just use the first available one
        cfg.model = loc[0].clone();
        println!("Switched to Ollama model: {}", cfg.model);
    } else {
        eprintln!("usage: yo switch <ollama|openai>");
        return;
    }
    save_config(&cfg);
    println!("switched to {}:{}", cfg.source, cfg.model);
    println!("⚙️ config saved at {}", get_config_path().display());
}

/// List available across both backends
pub async fn list_models() {
    let mut table = Table::new();
    table.add_row(Row::new(vec![Cell::new("Src"), Cell::new("Model"), Cell::new("You")]));

    let cfg = load_or_create_config();
    if let Some(key) = cfg.openai_api_key.as_deref() {
        for m in fetch_openai_models(key).await {
            let you = if cfg.source=="openai" && cfg.model==m { "✔" } else { "" };
            table.add_row(Row::new(vec![Cell::new("OpenAI"), Cell::new(&m), Cell::new(you)]));
        }
    }
    for m in fetch_ollama_local() {
        let you = if cfg.source=="ollama" && cfg.model==m { "✔" } else { "" };
        table.add_row(Row::new(vec![Cell::new("Ollama"), Cell::new(&m), Cell::new(you)]));
    }
    table.printstd();
}

/// Ask current model; for Ollama, use ollama run and exit with /bye
pub async fn ask(question: &[String]) {
    let chat_id = match get_current_chat_id() {
        Some(id) => id,
        None => { eprintln!("No current chat selected. Start or switch to a chat first."); return; }
    };
    let conn = match init_db() {
        Ok(c) => c,
        Err(e) => { eprintln!("DB error: {}", e); return; }
    };
    let prompt = question.join(" ");
    // Store user message
    let _ = conn.execute(
        "INSERT INTO messages (chat_id, role, content) VALUES (?1, 'user', ?2)",
        (&chat_id, &prompt),
    );
    let cfg = load_or_create_config();
    // Fetch full chat history for context
    let mut stmt = conn.prepare("SELECT role, content FROM messages WHERE chat_id = ?1 ORDER BY created_at ASC").unwrap();
    let history: Vec<(String, String)> = stmt
        .query_map([chat_id], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .flatten()
        .collect();
    match cfg.source.as_str() {
        "openai" => {
            let mut messages = vec![serde_json::json!({
                "role": "system",
                "content": "You are a helpful AI assistant."
            })];
            for (role, content) in &history {
                messages.push(serde_json::json!({"role": role, "content": content}));
            }
            messages.push(serde_json::json!({"role": "user", "content": &prompt}));
            let provider = Provider::OpenAI(OpenAIProvider {
                model: cfg.model.clone(),
                api_key: cfg.openai_api_key.clone().unwrap(),
            });
            match provider.ask_openai(messages).await {
                Ok(response) => {
                    let _ = conn.execute(
                        "INSERT INTO messages (chat_id, role, content) VALUES (?1, 'assistant', ?2)",
                        (&chat_id, &response),
                    );
                }
                Err(e) => {
                    eprintln!("Error during AI call: {}", e);
                }
            }
        }
        "ollama" => {
            let mut full_prompt = String::new();
            for (role, content) in &history {
                let who = match role.as_str() {
                    "user" => "User",
                    "assistant" => "AI",
                    _ => role.as_str(),
                };
                full_prompt.push_str(&format!("{}: {}\n", who, content));
            }
            full_prompt.push_str(&format!("User: {}\n", &prompt));
            let provider = Provider::Ollama(OllamaProvider {
                model: cfg.model.clone(),
            });
            match provider.ask_ollama(&full_prompt).await {
                Ok(response) => {
                    println!("{}", response);
                    let _ = conn.execute(
                        "INSERT INTO messages (chat_id, role, content) VALUES (?1, 'assistant', ?2)",
                        (&chat_id, &response),
                    );
                }
                Err(e) => {
                    eprintln!("Error during AI call: {}", e);
                }
            }
        }
        _ => eprintln!("Unknown backend: {}", cfg.source),
    }
}

/// Show information about the current model in use
pub fn show_current() {
    let cfg = load_or_create_config();
    
    println!("📋 Current AI Configuration");
    println!("---------------------------");
    println!("Backend: {}", cfg.source);
    println!("Model:   {}", cfg.model);
    
    if cfg.source == "ollama" {
        let output = ShellCommand::new("ollama")
            .args(["show", &cfg.model])
            .output();
            
        if let Ok(out) = output {
            let info = String::from_utf8_lossy(&out.stdout);
            if !info.is_empty() {
                let lines: Vec<&str> = info.lines().take(5).collect();
                if !lines.is_empty() {
                    println!("\nModel Details:");
                    for line in lines {
                        println!("  {}", line);
                    }
                }
            }
        }
    } else if cfg.source == "openai" {
        if let Some(api_key) = cfg.openai_api_key.as_deref() {
            if api_key.len() > 7 {
                let visible_part = &api_key[..7];
                let masked_part = "*".repeat(api_key.len() / 4);
                println!("\nAPI Key: {}{}", visible_part, masked_part);
            } else {
                println!("\nAPI Key: {}", "*".repeat(api_key.len()));
            }
        } else {
            println!("\nAPI Key: [not set]");
        }
    }
    
    println!("\n💡 Use 'yo list' to see all available models");
}

const CURRENT_CHAT_FILE: &str = "current_chat";

fn set_current_chat_id(chat_id: i64) {
    let config_dir = dirs::home_dir().unwrap().join(".config").join("yo");
    let file_path = config_dir.join(CURRENT_CHAT_FILE);
    let _ = fs::write(file_path, chat_id.to_string());
}

fn get_current_chat_id() -> Option<i64> {
    let config_dir = dirs::home_dir().unwrap().join(".config").join("yo");
    let file_path = config_dir.join(CURRENT_CHAT_FILE);
    if let Ok(s) = fs::read_to_string(file_path) {
        s.trim().parse().ok()
    } else {
        None
    }
}

pub fn new_chat(title: Option<String>) {
    let conn = match init_db() {
        Ok(c) => c,
        Err(e) => { eprintln!("DB error: {}", e); return; }
    };
    let title = title.unwrap_or_else(|| "New Chat".to_string());
    let res = conn.execute(
        "INSERT INTO chats (title) VALUES (?1)",
        [&title],
    );
    match res {
        Ok(_) => {
            let chat_id = conn.last_insert_rowid();
            set_current_chat_id(chat_id);
            println!("✅ Started new chat '{}' (id: {})", title, chat_id);
        },
        Err(e) => eprintln!("Failed to create chat: {}", e),
    }
}

pub fn list_chats() {
    let conn = match init_db() {
        Ok(c) => c,
        Err(e) => { eprintln!("DB error: {}", e); return; }
    };
    let mut stmt = match conn.prepare("SELECT id, title, created_at FROM chats ORDER BY created_at DESC") {
        Ok(s) => s,
        Err(e) => { eprintln!("Query error: {}", e); return; }
    };
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
    });
    match rows {
        Ok(rows) => {
            println!("\nChats:");
            for row in rows.flatten() {
                println!("  [{}] {} (created: {})", row.0, row.1, row.2);
            }
        },
        Err(e) => eprintln!("Failed to list chats: {}", e),
    }
}

pub fn switch_chat(chat_id: i64) {
    let conn = match init_db() {
        Ok(c) => c,
        Err(e) => { eprintln!("DB error: {}", e); return; }
    };
    let mut stmt = match conn.prepare("SELECT id, title FROM chats WHERE id = ?1") {
        Ok(s) => s,
        Err(e) => { eprintln!("Query error: {}", e); return; }
    };
    let result = stmt.query_row([chat_id], |row| {
        Ok(row.get::<_, String>(1)?)
    });
    match result {
        Ok(title) => {
            set_current_chat_id(chat_id);
            println!("✅ Switched to chat [{}] {}", chat_id, title);
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            eprintln!("Chat ID {} not found.", chat_id);
        }
        Err(e) => eprintln!("Failed to switch chat: {}", e),
    }
}

pub fn set_profile(pair: &str) {
    let parts: Vec<&str> = pair.splitn(2, '=').collect();
    if parts.len() != 2 {
        eprintln!("Invalid format. Use key=value");
        return;
    }
    let key = parts[0].trim();
    let value = parts[1].trim();
    let conn = match init_db() {
        Ok(c) => c,
        Err(e) => { eprintln!("DB error: {}", e); return; }
    };
    let res = conn.execute(
        "INSERT INTO user_profile (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [key, value],
    );
    match res {
        Ok(_) => println!("✅ Set profile: {} = {}", key, value),
        Err(e) => eprintln!("Failed to set profile: {}", e),
    }
}

pub fn summarize_chat(chat_id: i64) {
    let conn = match init_db() {
        Ok(c) => c,
        Err(e) => { eprintln!("DB error: {}", e); return; }
    };
    let mut stmt = match conn.prepare("SELECT role, content FROM messages WHERE chat_id = ?1 ORDER BY created_at ASC") {
        Ok(s) => s,
        Err(e) => { eprintln!("Query error: {}", e); return; }
    };
    let rows = stmt.query_map([chat_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    });
    match rows {
        Ok(rows) => {
            let mut full_chat = String::new();
            for row in rows.flatten() {
                let (role, content) = row;
                let who = match role.as_str() { "user" => "You", "assistant" => "AI", _ => &role };
                full_chat.push_str(&format!("{}: {}\n", who, content));
            }
            println!("\n--- Chat #{} Summary (stub) ---\n{}\n-------------------------------\n", chat_id, full_chat);
            // TODO: Send full_chat to AI with a 'summarize' prompt and print the result
        },
        Err(e) => eprintln!("Failed to summarize chat: {}", e),
    }
}

pub fn search_chats(query: &str) {
    let conn = match init_db() {
        Ok(c) => c,
        Err(e) => { eprintln!("DB error: {}", e); return; }
    };
    let sql = "SELECT chat_id, created_at, role, content FROM messages WHERE content LIKE ?1 ORDER BY chat_id, created_at";
    let pattern = format!("%{}%", query);
    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(e) => { eprintln!("Query error: {}", e); return; }
    };
    let rows = stmt.query_map([pattern], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?))
    });
    match rows {
        Ok(rows) => {
            println!("\n--- Search Results for '{}' ---", query);
            for row in rows.flatten() {
                let (chat_id, ts, role, content) = row;
                let who = match role.as_str() { "user" => "You", "assistant" => "AI", _ => &role };
                println!("[chat {}] [{}] {}: {}", chat_id, ts, who, content);
            }
            println!("-------------------------------\n");
        },
        Err(e) => eprintln!("Failed to search chats: {}", e),
    }
}

pub fn view_chat() {
    let chat_id = match get_current_chat_id() {
        Some(id) => id,
        None => { eprintln!("No current chat selected. Start or switch to a chat first."); return; }
    };
    let conn = match init_db() {
        Ok(c) => c,
        Err(e) => { eprintln!("DB error: {}", e); return; }
    };
    let mut stmt = match conn.prepare("SELECT created_at, role, content FROM messages WHERE chat_id = ?1 ORDER BY created_at ASC") {
        Ok(s) => s,
        Err(e) => { eprintln!("Query error: {}", e); return; }
    };
    let rows = stmt.query_map([chat_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
    });
    match rows {
        Ok(rows) => {
            println!("\n--- Chat History (chat id: {}) ---", chat_id);
            for row in rows.flatten() {
                let (ts, role, content) = row;
                let who = match role.as_str() { "user" => "You", "assistant" => "AI", _ => &role };
                println!("[{}] {}: {}", ts, who, content);
            }
            println!("-------------------------------\n");
        },
        Err(e) => eprintln!("Failed to view chat: {}", e),
    }
}

pub fn delete_chat(chat_id: i64) {
    println!("Are you sure you want to delete chat {}? This cannot be undone! (y/N): ", chat_id);
    io::stdout().flush().unwrap();
    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();
    if input.trim().to_lowercase() == "y" {
        let conn = match init_db() {
            Ok(c) => c,
            Err(e) => { eprintln!("DB error: {}", e); return; }
        };
        if let Err(e) = conn.execute("DELETE FROM messages WHERE chat_id = ?1", [chat_id]) {
            eprintln!("Failed to delete chat messages: {}", e);
        }
        if let Err(e) = conn.execute("DELETE FROM chats WHERE id = ?1", [chat_id]) {
            eprintln!("Failed to delete chat: {}", e);
        }
        println!("✅ Deleted chat {}", chat_id);
    } else {
        println!("Aborted.");
    }
}

pub fn clear_all_chats() {
    println!("Are you sure you want to delete ALL chats and messages? This cannot be undone! (y/N): ");
    io::stdout().flush().unwrap();
    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();
    if input.trim().to_lowercase() == "y" {
        let conn = match init_db() {
            Ok(c) => c,
            Err(e) => { eprintln!("DB error: {}", e); return; }
        };
        if let Err(e) = conn.execute("DELETE FROM messages", []) {
            eprintln!("Failed to clear messages: {}", e);
        }
        if let Err(e) = conn.execute("DELETE FROM chats", []) {
            eprintln!("Failed to clear chats: {}", e);
        }
        println!("✅ All chats and messages deleted");
    } else {
        println!("Aborted.");
    }
}

