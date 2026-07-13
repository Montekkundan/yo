mod cli;
mod config;
mod commands;
mod db;

use clap::Parser;
use cli::{Cli, Command};

#[tokio::main]
async fn main() {
    let args = Cli::parse();
    match args.command {
        Some(Command::Setup)            => commands::setup(),
        Some(Command::Config)           => commands::show_config_path(),
        Some(Command::Switch { model }) => commands::switch(&model).await,
        Some(Command::Gpt { model })    => commands::set_gpt(&model).await,
        Some(Command::List)             => commands::list_models().await,
        Some(Command::Ask { question }) => commands::ask(&question).await,
        Some(Command::Current)          => commands::show_current(),
        Some(Command::Other(o))         => commands::ask(&o).await,
        Some(Command::ClearHistory)     => commands::clear_history(),
        Some(Command::NewChat { title })      => commands::new_chat(title),
        Some(Command::ListChats)              => commands::list_chats(),
        Some(Command::SwitchChat { chat_id }) => commands::switch_chat(chat_id),
        Some(Command::SetProfile { pair })    => commands::set_profile(&pair),
        Some(Command::SummarizeChat { chat_id }) => commands::summarize_chat(chat_id),
        Some(Command::Search { query })       => commands::search_chats(&query),
        Some(Command::ViewChat)                 => commands::view_chat(),
        Some(Command::DeleteChat { chat_id })      => commands::delete_chat(chat_id),
        Some(Command::ClearAllChats)              => commands::clear_all_chats(),
        None                            => println!("yo what?"),
    }
}

