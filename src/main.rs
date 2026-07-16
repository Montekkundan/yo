use clap::Parser;
use yo::cli::{Cli, Command};
use yo::commands;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("yo: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    let args = Cli::parse();
    match args.command {
        Some(Command::Setup) => commands::setup().await?,
        Some(Command::Gateway(command)) => commands::gateway_command(command).await?,
        Some(Command::Config) => commands::show_config_paths(),
        Some(Command::Models) => commands::list_models().await?,
        Some(Command::Model { model }) => commands::set_model(&model).await?,
        Some(Command::Ask { question, private }) => commands::ask(&question, private).await?,
        Some(Command::Current) => commands::show_current()?,
        Some(Command::Permissions { mode, yes }) => commands::permissions(mode, yes)?,
        Some(Command::Run { command }) => commands::run_command(&command)?,
        Some(Command::Init { shell }) => commands::print_shell_init(shell),
        Some(Command::New { title }) => commands::new_chat(title)?,
        Some(Command::Chats) => commands::list_chats()?,
        Some(Command::Chat { chat_id }) => commands::switch_chat(chat_id)?,
        Some(Command::ViewChat) => commands::view_chat()?,
        Some(Command::DeleteChat { chat_id }) => commands::delete_chat(chat_id)?,
        Some(Command::ClearHistory) => commands::clear_history()?,
        Some(Command::ClearAllChats) => commands::clear_all_chats()?,
        Some(Command::Search { query }) => commands::search_chats(&query)?,
        Some(Command::Remember { text }) => commands::remember(&text).await?,
        Some(Command::Memory(command)) => commands::memory_command(command).await?,
        Some(Command::Personalize(command)) => commands::personalize_command(command)?,
        Some(Command::Settings) => commands::settings()?,
        Some(Command::Eval {
            filters,
            list,
            json,
            verbose,
            dir,
        }) => {
            yo::evals::run(yo::evals::EvalRunOptions {
                directory: dir,
                filters,
                list,
                json,
                verbose,
            })
            .await?
        }
        Some(Command::Other(question)) => commands::ask(&question, false).await?,
        None => println!("yo what? Try `yo --help`."),
    }
    Ok(())
}
