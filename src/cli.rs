use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "yo", about = "ask your terminal anything", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    #[command(about = "Ask your AI a question", 
        long_about = "Ask the currently configured AI model a question. The response will stream in real-time if using OpenAI.\n\nYou can use this command as either:\n  yo ask <question>\n  yo <question>\n\nBoth forms are equivalent.", 
        visible_alias = "a")]
    Ask {
        #[arg(required = true, help = "The question or prompt to send to the AI")]
        question: Vec<String>,
    },
    
    #[command(about = "Setup your AI configuration", long_about = "Interactive setup to configure your AI backend (Ollama or OpenAI) and select a default model.")]
    Setup,
    
    #[command(about = "Show config file path", long_about = "Print the path to the configuration file where your settings are stored.")]
    Config,
    
    #[command(about = "Switch AI backend", long_about = "Switch between Ollama and OpenAI backends, or select a different model for your current backend.")]
    Switch {
        /// either "openai" or "ollama"
        #[arg(help = "Backend to switch to: 'openai' or 'ollama'")]
        model: String,
    },
    
    #[command(about = "Set specific GPT model", long_about = "Directly set a specific OpenAI model without going through the selection menu.")]
    Gpt {
        /// specific GPT model to use, e.g. "gpt-3.5-turbo" or "gpt-4"
        #[arg(help = "OpenAI model name to use (e.g. 'gpt-4', 'gpt-3.5-turbo')")]
        model: String,
    },
    
    #[command(about = "List available AI models", long_about = "Show a list of all available models from both Ollama and OpenAI backends.")]
    List,
    
    #[command(about = "Show current AI model in use", long_about = "Display information about the currently selected AI backend and model.")]
    Current,
    
    #[command(about = "Clear the conversation history", long_about = "Clear the conversation history stored in history.txt.")]
    ClearHistory,
    
    #[command(external_subcommand)]
    Other(Vec<String>),
    
    #[command(about = "Start a new chat session", long_about = "Begin a new chat session and set it as the current chat.")]
    NewChat {
        #[arg(help = "Optional title for the new chat")]
        title: Option<String>,
    },

    #[command(about = "List all chat sessions", long_about = "List all chat sessions with their IDs and titles.")]
    ListChats,

    #[command(about = "Switch to a chat session", long_about = "Switch to a specific chat session by its ID.")]
    SwitchChat {
        #[arg(help = "ID of the chat to switch to")]
        chat_id: i64,
    },

    #[command(about = "Set a user profile key-value pair", long_about = "Set a key-value pair in the user profile (global memory). Format: key=value")]
    SetProfile {
        #[arg(help = "Key-value pair, e.g. name=Montek")]
        pair: String,
    },

    #[command(about = "Summarize a chat session", long_about = "Summarize the messages in a chat session by its ID.")]
    SummarizeChat {
        #[arg(help = "ID of the chat to summarize")]
        chat_id: i64,
    },

    #[command(about = "Search all chats for a keyword", long_about = "Search all chat messages for a given keyword.")]
    Search {
        #[arg(help = "Keyword to search for")]
        query: String,
    },

    #[command(about = "View the current chat's history", long_about = "Display all messages in the current chat session in a readable format.")]
    ViewChat,

    #[command(about = "Delete a chat session", long_about = "Delete a specific chat session and all its messages by its ID.")]
    DeleteChat {
        #[arg(help = "ID of the chat to delete")]
        chat_id: i64,
    },

    #[command(about = "Delete all chats and messages", long_about = "Delete all chat sessions and all messages. This cannot be undone.")]
    ClearAllChats,
}

