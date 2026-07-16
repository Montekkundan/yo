use clap::Parser;
use serial_test::serial;
use std::env;
use std::fs;
use std::path::PathBuf;
use uuid::Uuid;
use yo::cli::{Cli, Command, MemoryCommand, PermissionMode, PersonalizeCommand};
use yo::config::CommandConfirmation;
use yo::memory::{MemoryQuery, NewMemory};

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
        Some(Command::Setup)
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
#[serial]
fn config_never_serializes_provider_secrets() {
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
#[serial]
fn database_and_personalize_file_share_the_private_app_directory() {
    let _env = TestEnv::new();
    let _conn = yo::db::init_db().unwrap();
    let personalize = yo::personalize::ensure_exists().unwrap();
    assert_eq!(yo::db::get_db_path().parent(), personalize.parent());
    assert!(yo::db::get_db_path().exists());
    assert!(personalize.exists());
}

#[test]
#[serial]
fn memory_survives_database_reopen_and_a_new_terminal_session() {
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
#[serial]
fn full_access_permission_persists_without_serializing_secrets() {
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
