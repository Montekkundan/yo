use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Resolve $XDG_CONFIG_HOME or fallback to ~/.config
fn base_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else if let Some(home) = dirs::home_dir() {
        home.join(".config")
    } else {
        panic!("could not determine config directory");
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Config {
    /// "openai" or "ollama"
    pub source: String,
    /// model ID, e.g. "gpt-4" or "llama3:latest"
    pub model: String,
    pub openai_api_key: Option<String>,
}

pub fn get_config_path() -> PathBuf {
    let dir = base_dir().join("yo");
    fs::create_dir_all(&dir).unwrap();
    dir.join("config.toml")
}

pub fn load_or_create_config() -> Config {
    let path = get_config_path();
    if !path.exists() {
        eprintln!("no config found. run `yo setup` to get started.");
        std::process::exit(1);
    }
    let s = fs::read_to_string(&path).expect("failed to read config");
    toml::from_str(&s).expect("invalid config format")
}

pub fn save_config(cfg: &Config) {
    let path = get_config_path();
    let toml = toml::to_string_pretty(cfg).unwrap();
    fs::write(&path, toml).unwrap();
}

