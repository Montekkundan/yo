// Tests serialize process-environment mutation with `ENVIRONMENT_LOCK`. Rust
// exposes that mutation as unsafe because concurrent access would be unsound.
#![allow(unsafe_code, clippy::undocumented_unsafe_blocks)]

use clap::Parser;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use uuid::Uuid;
use yo::cli::{Cli, Command, MemoryCommand, PermissionMode, PersonalizeCommand};
use yo::config::CommandConfirmation;
use yo::memory::{MemoryQuery, NewMemory};

static ENVIRONMENT_LOCK: Mutex<()> = Mutex::new(());

fn environment_lock() -> MutexGuard<'static, ()> {
    ENVIRONMENT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct TestEnv {
    original: Option<String>,
    root: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        let original = env::var("XDG_CONFIG_HOME").ok();
        let root = env::temp_dir().join(format!("yo-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        unsafe { env::set_var("XDG_CONFIG_HOME", &root) };
        Self { original, root }
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        if let Some(value) = &self.original {
            unsafe { env::set_var("XDG_CONFIG_HOME", value) };
        } else {
            unsafe { env::remove_var("XDG_CONFIG_HOME") };
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn parses_gateway_only_commands() {
    assert!(matches!(
        Cli::try_parse_from(["yo", "setup"]).unwrap().command,
        Some(Command::Setup { provider: None })
    ));
    assert!(matches!(
        Cli::try_parse_from(["yo", "models"]).unwrap().command,
        Some(Command::Models)
    ));
    assert!(matches!(
        Cli::try_parse_from(["yo", "model", "anthropic/claude-sonnet-4.6"])
            .unwrap()
            .command,
        Some(Command::Model { .. })
    ));
}

#[test]
fn parses_private_ask() {
    let cli = Cli::try_parse_from(["yo", "ask", "--private", "hello"]).unwrap();
    match cli.command {
        Some(Command::Ask { question, private }) => {
            assert!(private);
            assert_eq!(question, ["hello"]);
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[test]
fn parses_direct_question() {
    let cli = Cli::try_parse_from(["yo", "what", "is", "the", "nvim", "command"]).unwrap();
    match cli.command {
        Some(Command::Other(words)) => assert_eq!(words[0], "what"),
        other => panic!("unexpected command: {other:?}"),
    }
}

#[test]
fn parses_explicit_command_execution() {
    let cli = Cli::try_parse_from(["yo", "run", "--", "nvm", "list"]).unwrap();
    match cli.command {
        Some(Command::Run { command }) => assert_eq!(command, ["nvm", "list"]),
        other => panic!("unexpected command: {other:?}"),
    }
}

#[test]
fn parses_memory_and_personalization_management() {
    let memory = Cli::try_parse_from(["yo", "memory", "search", "nvim"])
        .unwrap()
        .command;
    assert!(matches!(
        memory,
        Some(Command::Memory(MemoryCommand::Search { .. }))
    ));

    let personalize = Cli::try_parse_from(["yo", "personalize", "add", "be", "casual"])
        .unwrap()
        .command;
    assert!(matches!(
        personalize,
        Some(Command::Personalize(PersonalizeCommand::Add { .. }))
    ));
}

#[test]
fn parses_all_command_permission_modes() {
    for (name, expected) in [
        ("safe", PermissionMode::Safe),
        ("always-ask", PermissionMode::AlwaysAsk),
        ("full-access", PermissionMode::FullAccess),
    ] {
        let command = Cli::try_parse_from(["yo", "permissions", name])
            .unwrap()
            .command;
        assert!(matches!(
            command,
            Some(Command::Permissions {
                mode: Some(mode),
                ..
            }) if mode == expected
        ));
    }
}

#[test]
fn config_never_serializes_provider_secrets() {
    let _lock = environment_lock();
    let _env = TestEnv::new();
    let config = yo::config::Config {
        model: "anthropic/claude-sonnet-4.6".into(),
        ..yo::config::Config::default()
    };
    yo::config::save_config_result(&config).unwrap();
    let encoded = fs::read_to_string(yo::config::get_config_path()).unwrap();
    assert!(!encoded.to_lowercase().contains("api_key"));
    assert!(!encoded.to_lowercase().contains("ollama"));
    assert!(encoded.contains("anthropic/claude-sonnet-4.6"));
}

#[test]
fn database_backup_and_repair_preserve_a_valid_database() {
    let _lock = environment_lock();
    let env = TestEnv::new();
    let conn = yo::db::init_db().unwrap();
    yo::db::ensure_session(&conn, "backup-test", "/bin/sh", None, "/tmp", None).unwrap();
    drop(conn);

    #[cfg(unix)]
    let original_parent_mode = {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&env.root, fs::Permissions::from_mode(0o755)).unwrap();
        fs::metadata(&env.root).unwrap().permissions().mode() & 0o777
    };
    let explicit = env.root.join("manual-backup.db");
    assert_eq!(yo::db::backup_database(Some(&explicit)).unwrap(), explicit);
    assert!(explicit.is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&env.root).unwrap().permissions().mode() & 0o777,
            original_parent_mode,
            "an explicit backup must not chmod its existing parent"
        );
        assert_eq!(
            fs::metadata(&explicit).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert_eq!(yo::db::integrity_check().unwrap(), "ok");

    let report = yo::db::repair_database().unwrap();
    assert!(report.backup.is_file());
    assert_eq!(report.integrity_before, "ok");
    assert_eq!(report.integrity_after, "ok");
}

#[test]
fn diagnostics_are_opt_in_and_store_only_bounded_metadata() {
    let _lock = environment_lock();
    let _env = TestEnv::new();
    yo::diagnostics::record(
        yo::diagnostics::DiagnosticEvent::new("tool.command").outcome(true, 12),
    );
    assert!(yo::diagnostics::read_events().unwrap().is_empty());

    let config = yo::config::Config {
        diagnostics_enabled: true,
        ..yo::config::Config::default()
    };
    yo::config::save_config_result(&config).unwrap();
    yo::diagnostics::record(
        yo::diagnostics::DiagnosticEvent::new("tool.command").outcome(true, 12),
    );
    let events = yo::diagnostics::read_events().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "tool.command");
    let encoded = fs::read_to_string(yo::diagnostics::path()).unwrap();
    assert!(!encoded.contains("stdout"));
    assert!(!encoded.contains("prompt"));
}

#[test]
fn database_and_personalize_file_share_the_private_app_directory() {
    let _lock = environment_lock();
    let _env = TestEnv::new();
    let _conn = yo::db::init_db().unwrap();
    let personalize = yo::personalize::ensure_exists().unwrap();
    assert_eq!(yo::db::get_db_path().parent(), personalize.parent());
    assert!(yo::db::get_db_path().exists());
    assert!(personalize.exists());
}

#[test]
fn memory_survives_database_reopen_and_a_new_terminal_session() {
    let _lock = environment_lock();
    let _env = TestEnv::new();
    {
        let conn = yo::db::init_db().unwrap();
        yo::memory::init_memory_schema(&conn).unwrap();
        yo::db::ensure_session(&conn, "terminal-one", "zsh", None, "/tmp", None).unwrap();
        yo::memory::add_memory(
            &conn,
            &NewMemory::global("cross-session retention token cobalt719"),
        )
        .unwrap();
    }

    let conn = yo::db::init_db().unwrap();
    yo::memory::init_memory_schema(&conn).unwrap();
    yo::db::ensure_session(&conn, "terminal-two", "zsh", None, "/tmp", None).unwrap();
    let found = yo::memory::search_memories(&conn, &MemoryQuery::text("cobalt719")).unwrap();
    assert_eq!(found.len(), 1);
    assert!(found[0].memory.text.contains("cobalt719"));
}

#[test]
fn full_access_permission_persists_without_serializing_secrets() {
    let _lock = environment_lock();
    let _env = TestEnv::new();
    let config = yo::config::Config {
        command_confirmation: CommandConfirmation::FullAccess,
        ..yo::config::Config::default()
    };
    yo::config::save_config_result(&config).unwrap();

    let reloaded = yo::config::load_config().unwrap();
    assert_eq!(
        reloaded.command_confirmation,
        CommandConfirmation::FullAccess
    );
    let encoded = fs::read_to_string(yo::config::get_config_path()).unwrap();
    assert!(encoded.contains("command_confirmation = \"full-access\""));
    assert!(!encoded.to_lowercase().contains("api_key"));
}
