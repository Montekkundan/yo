use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

const KEYRING_SERVICE: &str = "yo-ai-gateway";
const KEYRING_ACCOUNT: &str = "default";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    /// Vercel AI Gateway model ID in `creator/model` form.
    pub model: String,
    pub embedding_model: String,
    pub memory_enabled: bool,
    pub auto_memory: bool,
    pub terminal_context_enabled: bool,
    pub max_terminal_output_bytes: usize,
    pub max_history_messages: usize,
    pub command_confirmation: CommandConfirmation,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CommandConfirmation {
    /// Explicit `yo run -- ...` executes immediately. A recognized read-only
    /// model command executes immediately only when the user includes that exact
    /// command in the request; unknown or mutating model commands still require
    /// confirmation.
    #[default]
    Smart,
    /// Ask before every command proposed by the model.
    Always,
    /// Never ask before a model-proposed command. Commands still run with the
    /// user's normal permissions, output limits, redaction, and timeout.
    FullAccess,
}

impl CommandConfirmation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Smart => "safe",
            Self::Always => "always-ask",
            Self::FullAccess => "full-access",
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: String::new(),
            embedding_model: "openai/text-embedding-3-small".into(),
            memory_enabled: true,
            auto_memory: true,
            terminal_context_enabled: true,
            max_terminal_output_bytes: 16 * 1024,
            max_history_messages: 24,
            command_confirmation: CommandConfirmation::Smart,
        }
    }
}

pub fn get_app_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(dirs::config_dir)
        .or_else(|| dirs::home_dir().map(|home| home.join(".config")))
        .expect("could not determine a configuration directory");
    let dir = base.join("yo");
    if let Err(error) = create_private_dir(&dir) {
        eprintln!("warning: could not prepare {}: {error}", dir.display());
    }
    dir
}

pub fn get_config_path() -> PathBuf {
    get_app_dir().join("config.toml")
}

pub fn load_config() -> Result<Config> {
    load_config_from(&get_config_path())
}

pub fn load_or_create_config() -> Config {
    load_config().unwrap_or_else(|error| {
        eprintln!("warning: could not load config: {error}");
        Config::default()
    })
}

fn load_config_from(path: &Path) -> Result<Config> {
    if !path.exists() {
        return Ok(Config::default());
    }

    let source =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let raw: toml::Value = toml::from_str(&source)
        .with_context(|| format!("invalid configuration in {}", path.display()))?;
    let had_legacy_secret = raw.get("openai_api_key").is_some();
    let had_legacy_source = raw.get("source").is_some();

    let mut config = Config::default();
    if let Some(model) = raw.get("model").and_then(toml::Value::as_str) {
        // Old direct-provider model names are not valid Gateway IDs. Do not
        // silently guess a provider; setup will offer the live model list.
        if model.contains('/') {
            config.model = model.to_owned();
        }
    }
    if let Some(model) = raw.get("embedding_model").and_then(toml::Value::as_str) {
        config.embedding_model = model.to_owned();
    }
    if let Some(value) = raw.get("memory_enabled").and_then(toml::Value::as_bool) {
        config.memory_enabled = value;
    }
    if let Some(value) = raw.get("auto_memory").and_then(toml::Value::as_bool) {
        config.auto_memory = value;
    }
    if let Some(value) = raw
        .get("terminal_context_enabled")
        .and_then(toml::Value::as_bool)
    {
        config.terminal_context_enabled = value;
    }
    if let Some(value) = raw
        .get("max_terminal_output_bytes")
        .and_then(toml::Value::as_integer)
        .and_then(|value| usize::try_from(value).ok())
    {
        config.max_terminal_output_bytes = value.max(1024);
    }
    if let Some(value) = raw
        .get("max_history_messages")
        .and_then(toml::Value::as_integer)
        .and_then(|value| usize::try_from(value).ok())
    {
        config.max_history_messages = value.max(4);
    }
    if let Some(value) = raw
        .get("command_confirmation")
        .and_then(toml::Value::as_str)
    {
        config.command_confirmation = match value {
            "always" | "always-ask" => CommandConfirmation::Always,
            "full-access" => CommandConfirmation::FullAccess,
            _ => CommandConfirmation::Smart,
        };
    }

    if had_legacy_secret || had_legacy_source {
        backup_legacy_config(path, &raw)?;
        save_config_to(path, &config)?;
    }
    Ok(config)
}

fn backup_legacy_config(path: &Path, raw: &toml::Value) -> Result<()> {
    let backup = path.with_extension("toml.pre-gateway.bak");
    if backup.exists() {
        return Ok(());
    }
    let mut sanitized = raw.clone();
    if let Some(table) = sanitized.as_table_mut() {
        table.remove("openai_api_key");
    }
    let encoded = toml::to_string_pretty(&sanitized).context("failed to encode config backup")?;
    fs::write(&backup, encoded)
        .with_context(|| format!("failed to back up legacy config to {}", backup.display()))?;
    set_private_file_permissions(&backup)?;
    Ok(())
}

pub fn save_config(config: &Config) {
    if let Err(error) = save_config_result(config) {
        eprintln!("failed to save configuration: {error}");
    }
}

pub fn save_config_result(config: &Config) -> Result<()> {
    save_config_to(&get_config_path(), config)
}

fn save_config_to(path: &Path, config: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let encoded = toml::to_string_pretty(config).context("failed to encode configuration")?;
    let temporary = path.with_extension("toml.tmp");
    fs::write(&temporary, encoded)
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    set_private_file_permissions(&temporary)?;
    fs::rename(&temporary, path)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    set_private_file_permissions(path)?;
    Ok(())
}

/// Credential lookup order matches Vercel's CLI guidance: an explicit Gateway
/// key, then an OIDC token, then the native credential store.
pub fn gateway_credential() -> Result<String> {
    for name in ["AI_GATEWAY_API_KEY", "VERCEL_OIDC_TOKEN"] {
        if let Ok(value) = std::env::var(name) {
            if !value.trim().is_empty() {
                return Ok(value);
            }
        }
    }
    keyring_entry()?
        .get_password()
        .context("no Vercel AI Gateway credential found; run `yo setup` or set AI_GATEWAY_API_KEY")
}

pub fn store_gateway_credential(secret: &str) -> Result<()> {
    if secret.trim().is_empty() {
        anyhow::bail!("the Gateway credential cannot be empty");
    }
    keyring_entry()?
        .set_password(secret.trim())
        .context("failed to save the Gateway credential in the OS credential store")
}

pub fn delete_gateway_credential() -> Result<()> {
    keyring_entry()?
        .delete_credential()
        .context("failed to delete the Gateway credential from the OS credential store")
}

pub fn gateway_credential_source() -> &'static str {
    if std::env::var_os("AI_GATEWAY_API_KEY").is_some() {
        "AI_GATEWAY_API_KEY"
    } else if std::env::var_os("VERCEL_OIDC_TOKEN").is_some() {
        "VERCEL_OIDC_TOKEN"
    } else if gateway_credential().is_ok() {
        "OS credential store"
    } else {
        "not configured"
    }
}

fn keyring_entry() -> Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)
        .context("failed to open the OS credential store")
}

fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn temporary_path() -> PathBuf {
        std::env::temp_dir().join(format!("yo-config-{}.toml", Uuid::new_v4()))
    }

    #[test]
    fn legacy_plaintext_credentials_are_removed_during_migration() {
        let path = temporary_path();
        fs::write(
            &path,
            "source = \"openai\"\nmodel = \"gpt-4\"\nopenai_api_key = \"secret\"\n",
        )
        .unwrap();

        let config = load_config_from(&path).unwrap();
        assert!(config.model.is_empty());
        let migrated = fs::read_to_string(&path).unwrap();
        let backup = fs::read_to_string(path.with_extension("toml.pre-gateway.bak")).unwrap();
        assert!(!backup.contains("secret"));
        assert!(!migrated.contains("source"));
        assert!(!migrated.contains("openai_api_key"));
        assert!(!migrated.contains("secret"));
        let _ = fs::remove_file(path.with_extension("toml.pre-gateway.bak"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn gateway_model_survives_legacy_migration() {
        let path = temporary_path();
        fs::write(
            &path,
            "source = \"openai\"\nmodel = \"anthropic/claude-sonnet-4.6\"\n",
        )
        .unwrap();
        let config = load_config_from(&path).unwrap();
        assert_eq!(config.model, "anthropic/claude-sonnet-4.6");
        let _ = fs::remove_file(path.with_extension("toml.pre-gateway.bak"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn configuration_can_be_replaced_repeatedly() {
        let path = temporary_path();
        let mut config = Config {
            model: "anthropic/claude-sonnet-4.6".into(),
            ..Config::default()
        };
        save_config_to(&path, &config).unwrap();
        config.model = "openai/gpt-5.5".into();
        save_config_to(&path, &config).unwrap();
        assert_eq!(load_config_from(&path).unwrap(), config);
        let _ = fs::remove_file(path);
    }
}
