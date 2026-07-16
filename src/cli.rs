use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "yo", about = "your personal AI terminal assistant", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    #[command(visible_alias = "a", about = "Ask Yo a question")]
    Ask {
        #[arg(long, help = "Do not store or recall durable memory for this turn")]
        private: bool,
        #[arg(required = true)]
        question: Vec<String>,
    },

    #[command(about = "Configure a Vercel AI Gateway credential and model")]
    Setup,

    #[command(subcommand, about = "Manage the Vercel AI Gateway credential")]
    Gateway(GatewayCommand),

    #[command(about = "Show Yo's local configuration paths")]
    Config,

    #[command(
        visible_alias = "list",
        about = "List models available through AI Gateway"
    )]
    Models,

    #[command(about = "Select a Vercel AI Gateway model")]
    Model { model: String },

    #[command(about = "Show the active model, session, and feature status")]
    Current,

    #[command(about = "Show or change model command approval behavior")]
    Permissions {
        #[arg(value_enum)]
        mode: Option<PermissionMode>,
        #[arg(long, help = "Confirm the full-access warning non-interactively")]
        yes: bool,
    },

    #[command(
        about = "Run a command, capture its result, and save it as terminal context",
        trailing_var_arg = true
    )]
    Run {
        #[arg(required = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },

    #[command(about = "Print shell integration for a new terminal session")]
    Init { shell: ShellKind },

    #[command(
        visible_alias = "new-chat",
        about = "Start a new chat in this terminal"
    )]
    New {
        #[arg(short, long)]
        title: Option<String>,
    },

    #[command(visible_alias = "list-chats", about = "List saved chats")]
    Chats,

    #[command(
        visible_alias = "switch-chat",
        about = "Use a saved chat in this terminal"
    )]
    Chat { chat_id: i64 },

    #[command(about = "View the current terminal chat")]
    ViewChat,

    #[command(about = "Delete one chat and its messages")]
    DeleteChat { chat_id: i64 },

    #[command(about = "Clear the current chat history")]
    ClearHistory,

    #[command(about = "Delete all local chats")]
    ClearAllChats,

    #[command(about = "Search chat messages")]
    Search { query: Vec<String> },

    #[command(about = "Store a durable memory explicitly")]
    Remember { text: Vec<String> },

    #[command(subcommand, about = "Inspect and manage cross-session memory")]
    Memory(MemoryCommand),

    #[command(subcommand, about = "Manage personalize.md response instructions")]
    Personalize(PersonalizeCommand),

    #[command(about = "Open the native terminal settings interface")]
    Settings,

    #[command(about = "Run file-based agent workflow evals")]
    Eval {
        #[arg(value_name = "FILTER", help = "Eval id or directory prefix to run")]
        filters: Vec<String>,
        #[arg(long, help = "List discovered evals without running them")]
        list: bool,
        #[arg(long, help = "Print a machine-readable JSON summary")]
        json: bool,
        #[arg(
            long,
            help = "Include replies and command output in the console report"
        )]
        verbose: bool,
        #[arg(long, default_value = "evals", value_name = "PATH")]
        dir: PathBuf,
    },

    #[command(external_subcommand)]
    Other(Vec<String>),
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ShellKind {
    Zsh,
    Bash,
    Fish,
}

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
pub enum PermissionMode {
    /// Auto-run only an exact, recognized read-only command; ask for everything else.
    Safe,
    /// Ask before every command proposed by the model.
    AlwaysAsk,
    /// Let the model run commands without approval prompts.
    FullAccess,
}

#[derive(Subcommand, Debug)]
pub enum MemoryCommand {
    List,
    Search { query: Vec<String> },
    Edit { id: i64, text: Vec<String> },
    Forget { id: i64 },
    Clear,
    Purge,
    Export,
    Reindex,
    On,
    Off,
}

#[derive(Subcommand, Debug)]
pub enum PersonalizeCommand {
    Show,
    Path,
    Edit,
    Add { instruction: Vec<String> },
    Reset,
}

#[derive(Subcommand, Debug)]
pub enum GatewayCommand {
    Status,
    Set,
    Delete,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_question_is_an_external_subcommand() {
        let cli = Cli::try_parse_from(["yo", "what", "is", "nvm"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Other(_))));
    }

    #[test]
    fn run_accepts_flags_after_separator() {
        let cli = Cli::try_parse_from(["yo", "run", "--", "cargo", "test", "--all"]).unwrap();
        match cli.command {
            Some(Command::Run { command }) => {
                assert_eq!(command, ["cargo", "test", "--all"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn memory_search_parses_multiple_words() {
        let cli = Cli::try_parse_from(["yo", "memory", "search", "nvim", "command"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Memory(MemoryCommand::Search { .. }))
        ));
    }

    #[test]
    fn gateway_credential_management_parses() {
        let cli = Cli::try_parse_from(["yo", "gateway", "delete"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Gateway(GatewayCommand::Delete))
        ));
    }

    #[test]
    fn command_permission_modes_parse() {
        let cli = Cli::try_parse_from(["yo", "permissions", "full-access", "--yes"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Permissions {
                mode: Some(PermissionMode::FullAccess),
                yes: true
            })
        ));
    }

    #[test]
    fn eval_options_parse() {
        let cli =
            Cli::try_parse_from(["yo", "eval", "terminal", "--json", "--dir", "checks"]).unwrap();
        match cli.command {
            Some(Command::Eval {
                filters,
                list,
                json,
                verbose,
                dir,
            }) => {
                assert_eq!(filters, ["terminal"]);
                assert!(!list);
                assert!(json);
                assert!(!verbose);
                assert_eq!(dir, PathBuf::from("checks"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
