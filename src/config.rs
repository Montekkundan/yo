use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

const KEYRING_SERVICE: &str = "yo-ai-gateway";
const KEYRING_ACCOUNT: &str = "default";
const PROVIDER_KEYRING_SERVICE: &str = "yo-gateway";

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum GatewayProvider {
    #[default]
    Vercel,
    LlmGateway,
    OpenRouter,
}

impl GatewayProvider {
    pub const ALL: [Self; 3] = [Self::Vercel, Self::LlmGateway, Self::OpenRouter];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vercel => "vercel",
            Self::LlmGateway => "llmgateway",
            Self::OpenRouter => "openrouter",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Vercel => "Vercel AI Gateway",
            Self::LlmGateway => "LLM Gateway",
            Self::OpenRouter => "OpenRouter",
        }
    }

    pub fn base_url(self) -> &'static str {
        match self {
            Self::Vercel => "https://ai-gateway.vercel.sh/v1",
            Self::LlmGateway => "https://api.llmgateway.io/v1",
            Self::OpenRouter => "https://openrouter.ai/api/v1",
        }
    }

    pub fn key_url(self) -> &'static str {
        match self {
            Self::Vercel => "https://vercel.com/ai-gateway",
            Self::LlmGateway => "https://llmgateway.io",
            Self::OpenRouter => "https://openrouter.ai/keys",
        }
    }

    pub fn environment_variables(self) -> &'static [&'static str] {
        match self {
            Self::Vercel => &["AI_GATEWAY_API_KEY", "VERCEL_OIDC_TOKEN"],
            Self::LlmGateway => &["LLM_GATEWAY_API_KEY", "LLMGATEWAY_API_KEY"],
            Self::OpenRouter => &["OPENROUTER_API_KEY"],
        }
    }
}

impl std::fmt::Display for GatewayProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.display_name())
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    #[default]
    Auto,
    Required,
    Off,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    pub gateway_provider: GatewayProvider,
    /// Model ID as returned by the selected gateway.
    pub model: String,
    pub embedding_model: String,
    pub memory_enabled: bool,
    pub auto_memory: bool,
    pub terminal_context_enabled: bool,
    pub max_terminal_output_bytes: usize,
    pub max_history_messages: usize,
    pub command_confirmation: CommandConfirmation,
    pub sandbox_mode: SandboxMode,
    pub sandbox_network: bool,
    pub sandbox_read_paths: Vec<PathBuf>,
    pub sandbox_write_paths: Vec<PathBuf>,
    pub diagnostics_enabled: bool,
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
            gateway_provider: GatewayProvider::Vercel,
            model: String::new(),
            embedding_model: "openai/text-embedding-3-small".into(),
            memory_enabled: true,
            auto_memory: true,
            terminal_context_enabled: true,
            max_terminal_output_bytes: 16 * 1024,
            max_history_messages: 24,
            command_confirmation: CommandConfirmation::Smart,
            sandbox_mode: SandboxMode::Auto,
            sandbox_network: false,
            sandbox_read_paths: Vec::new(),
            sandbox_write_paths: Vec::new(),
            diagnostics_enabled: false,
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

pub fn load_or_create_config() -> Result<Config> {
    load_config()
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
    if let Some(provider) = raw.get("gateway_provider").and_then(toml::Value::as_str) {
        config.gateway_provider = match provider {
            "vercel" => GatewayProvider::Vercel,
            "llmgateway" | "llm-gateway" => GatewayProvider::LlmGateway,
            "openrouter" | "open-router" => GatewayProvider::OpenRouter,
            other => anyhow::bail!(
                "invalid gateway_provider {other:?} in {}; expected vercel, llm-gateway, or openrouter",
                path.display()
            ),
        };
    }
    if let Some(model) = raw.get("model").and_then(toml::Value::as_str) {
        // Old direct-provider model names are not valid Gateway IDs. Do not
        // silently guess a provider; setup will offer the live model list.
        if !model.trim().is_empty() && (!had_legacy_source || model.contains('/')) {
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
            "smart" | "safe" => CommandConfirmation::Smart,
            "always" | "always-ask" => CommandConfirmation::Always,
            "full-access" => CommandConfirmation::FullAccess,
            other => anyhow::bail!(
                "invalid command_confirmation {other:?} in {}; expected smart, always, or full-access",
                path.display()
            ),
        };
    }
    if let Some(value) = raw.get("sandbox_mode").and_then(toml::Value::as_str) {
        config.sandbox_mode = match value {
            "auto" => SandboxMode::Auto,
            "required" => SandboxMode::Required,
            "off" => SandboxMode::Off,
            other => anyhow::bail!(
                "invalid sandbox_mode {other:?} in {}; expected auto, required, or off",
                path.display()
            ),
        };
    }
    if let Some(value) = raw.get("sandbox_network").and_then(toml::Value::as_bool) {
        config.sandbox_network = value;
    }
    if let Some(values) = raw
        .get("sandbox_read_paths")
        .and_then(toml::Value::as_array)
    {
        config.sandbox_read_paths = values
            .iter()
            .filter_map(toml::Value::as_str)
            .map(PathBuf::from)
            .collect();
    }
    if let Some(values) = raw
        .get("sandbox_write_paths")
        .and_then(toml::Value::as_array)
    {
        config.sandbox_write_paths = values
            .iter()
            .filter_map(toml::Value::as_str)
            .map(PathBuf::from)
            .collect();
    }
    if let Some(value) = raw
        .get("diagnostics_enabled")
        .and_then(toml::Value::as_bool)
    {
        config.diagnostics_enabled = value;
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
pub fn gateway_credential_for(provider: GatewayProvider) -> Result<String> {
    for name in provider.environment_variables() {
        if let Ok(value) = std::env::var(name) {
            if !value.trim().is_empty() {
                return Ok(value);
            }
        }
    }
    if let Ok(value) = provider_keyring_entry(provider)?.get_password() {
        return Ok(value);
    }
    if provider == GatewayProvider::Vercel {
        if let Ok(value) = legacy_keyring_entry()?.get_password() {
            return Ok(value);
        }
    }
    anyhow::bail!(
        "no {} credential found; run `yo setup` or set {}",
        provider.display_name(),
        provider.environment_variables()[0]
    )
}

pub fn gateway_credential() -> Result<String> {
    gateway_credential_for(load_or_create_config()?.gateway_provider)
}

pub fn store_gateway_credential_for(provider: GatewayProvider, secret: &str) -> Result<()> {
    if secret.trim().is_empty() {
        anyhow::bail!("the Gateway credential cannot be empty");
    }
    provider_keyring_entry(provider)?
        .set_password(secret.trim())
        .context("failed to save the Gateway credential in the OS credential store")
}

pub fn store_gateway_credential(secret: &str) -> Result<()> {
    store_gateway_credential_for(load_or_create_config()?.gateway_provider, secret)
}

pub fn delete_gateway_credential_for(provider: GatewayProvider) -> Result<()> {
    provider_keyring_entry(provider)?
        .delete_credential()
        .context("failed to delete the Gateway credential from the OS credential store")
}

pub fn delete_gateway_credential() -> Result<()> {
    delete_gateway_credential_for(load_or_create_config()?.gateway_provider)
}

pub fn gateway_credential_source_for(provider: GatewayProvider) -> String {
    if let Some(name) = provider
        .environment_variables()
        .iter()
        .find(|name| std::env::var_os(name).is_some())
    {
        (*name).to_owned()
    } else if gateway_credential_for(provider).is_ok() {
        "OS credential store".to_owned()
    } else {
        "not configured".to_owned()
    }
}

pub fn gateway_credential_source() -> Result<String> {
    let provider = load_or_create_config()?.gateway_provider;
    Ok(gateway_credential_source_for(provider))
}

fn provider_keyring_entry(provider: GatewayProvider) -> Result<keyring::Entry> {
    keyring::Entry::new(PROVIDER_KEYRING_SERVICE, provider.as_str())
        .context("failed to open the OS credential store")
}

fn legacy_keyring_entry() -> Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)
        .context("failed to open the legacy OS credential store")
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

fn set_private_file_permissions(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(_path, fs::Permissions::from_mode(0o600))?;
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

    #[test]
    fn gateway_provider_and_unprefixed_model_round_trip() {
        let path = temporary_path();
        let config = Config {
            gateway_provider: GatewayProvider::LlmGateway,
            model: "gpt-5.4-mini".into(),
            ..Config::default()
        };
        save_config_to(&path, &config).unwrap();
        assert_eq!(load_config_from(&path).unwrap(), config);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn invalid_security_sensitive_values_are_rejected() {
        for source in [
            "gateway_provider = \"unknown\"\n",
            "command_confirmation = \"full-acess\"\n",
            "sandbox_mode = \"requred\"\n",
        ] {
            let path = temporary_path();
            fs::write(&path, source).unwrap();
            let error = load_config_from(&path).unwrap_err().to_string();
            assert!(error.contains("invalid"), "unexpected error: {error}");
            let _ = fs::remove_file(path);
        }
    }
}
