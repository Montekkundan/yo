use crate::{config, db, memory, personalize};
use anyhow::{Context, Result};
use crossterm::cursor::Show;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Tabs, Wrap};
use ratatui::Terminal;
use rusqlite::Connection;
use std::io::{self, IsTerminal};

const TAB_NAMES: [&str; 4] = ["Overview", "Chats", "Memory", "Personalize"];

pub fn run() -> Result<()> {
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        anyhow::bail!("`yo settings` needs an interactive terminal");
    }

    loop {
        match run_once()? {
            ExitAction::Quit => return Ok(()),
            ExitAction::EditPersonalize => personalize::edit()?,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExitAction {
    Quit,
    EditPersonalize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingConfirmation {
    Delete { tab: usize, id: i64 },
    FullAccess,
}

fn run_once() -> Result<ExitAction> {
    let guard = TerminalModeGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let result = event_loop(&mut terminal);
    drop(terminal);
    drop(guard);
    result
}

struct TerminalModeGuard;

impl TerminalModeGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("failed to enable raw terminal mode")?;
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error).context("failed to enter the alternate terminal screen");
        }
        Ok(Self)
    }
}

impl Drop for TerminalModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
    }
}

fn event_loop(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<ExitAction> {
    let conn = db::init_db()?;
    memory::init_memory_schema(&conn)?;
    let mut tab = 0usize;
    let mut selected = 0usize;
    let mut confirmation: Option<PendingConfirmation> = None;

    loop {
        let config = config::load_or_create_config()?;
        let chats = db::list_chats(&conn).unwrap_or_default();
        let memories = load_memories(&conn);
        let personalization = personalize::load().unwrap_or_default();

        terminal.draw(|frame| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(5),
                    Constraint::Length(3),
                ])
                .split(frame.area());

            let titles = TAB_NAMES
                .iter()
                .map(|title| Line::from(*title))
                .collect::<Vec<_>>();
            let tabs = Tabs::new(titles)
                .select(tab)
                .block(Block::default().title(" Yo settings ").borders(Borders::ALL))
                .highlight_style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                );
            frame.render_widget(tabs, chunks[0]);

            match tab {
                0 => render_overview(frame, chunks[1], &config),
                1 => {
                    let items = chats
                        .iter()
                        .enumerate()
                        .map(|(index, chat)| {
                            let marker = if index == selected { "›" } else { " " };
                            ListItem::new(format!(
                                "{marker} [{}] {}  {}",
                                chat.id, chat.title, chat.updated_at
                            ))
                        })
                        .collect::<Vec<_>>();
                    frame.render_widget(
                        List::new(items).block(
                            Block::default()
                                .title(" Chats (d twice to delete) ")
                                .borders(Borders::ALL),
                        ),
                        chunks[1],
                    );
                }
                2 => {
                    let items = memories
                        .iter()
                        .enumerate()
                        .map(|(index, (id, text, pinned))| {
                            let marker = if index == selected { "›" } else { " " };
                            let pin = if *pinned { "★" } else { " " };
                            ListItem::new(format!("{marker} {pin} [{id}] {text}"))
                        })
                        .collect::<Vec<_>>();
                    frame.render_widget(
                        List::new(items).block(
                            Block::default()
                                .title(" Memories (d twice to forget) ")
                                .borders(Borders::ALL),
                        ),
                        chunks[1],
                    );
                }
                _ => frame.render_widget(
                    Paragraph::new(Text::from(personalization))
                        .block(
                            Block::default()
                                .title(" personalize.md (e to edit) ")
                                .borders(Borders::ALL),
                        )
                        .wrap(Wrap { trim: false }),
                    chunks[1],
                ),
            }

            let confirmation_text = confirmation.map_or(String::new(), |pending| match pending {
                PendingConfirmation::Delete { .. } => {
                    "  Press d again to confirm deletion, or any other key to cancel.".into()
                }
                PendingConfirmation::FullAccess => {
                    "  Full access runs model commands without asking. Press a again to confirm.".into()
                }
            });
            let help = format!(
                " ←/→ tabs  ↑/↓ select  a approvals  m memory  t terminal context  e edit personalize  q quit{confirmation_text}"
            );
            frame.render_widget(
                Paragraph::new(help)
                    .style(Style::default().fg(Color::DarkGray))
                    .block(Block::default().borders(Borders::ALL)),
                chunks[2],
            );
        })?;

        if !event::poll(std::time::Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        if let Some(pending) = confirmation {
            let expected = match pending {
                PendingConfirmation::Delete { .. } => KeyCode::Char('d'),
                PendingConfirmation::FullAccess => KeyCode::Char('a'),
            };
            if key.code != expected {
                confirmation = None;
            }
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(ExitAction::Quit),
            KeyCode::Right | KeyCode::Tab => {
                tab = (tab + 1) % TAB_NAMES.len();
                selected = 0;
                confirmation = None;
            }
            KeyCode::Left | KeyCode::BackTab => {
                tab = (tab + TAB_NAMES.len() - 1) % TAB_NAMES.len();
                selected = 0;
                confirmation = None;
            }
            KeyCode::Down => {
                let length = if tab == 1 {
                    chats.len()
                } else if tab == 2 {
                    memories.len()
                } else {
                    0
                };
                if length > 0 {
                    selected = (selected + 1).min(length - 1);
                }
            }
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Char('m') => {
                let mut value = config;
                value.memory_enabled = !value.memory_enabled;
                config::save_config_result(&value)?;
            }
            KeyCode::Char('t') => {
                let mut value = config;
                value.terminal_context_enabled = !value.terminal_context_enabled;
                config::save_config_result(&value)?;
            }
            KeyCode::Char('a') => {
                let mut value = config;
                match value.command_confirmation {
                    config::CommandConfirmation::Smart => {
                        value.command_confirmation = config::CommandConfirmation::Always;
                        confirmation = None;
                        config::save_config_result(&value)?;
                    }
                    config::CommandConfirmation::Always
                        if confirmation == Some(PendingConfirmation::FullAccess) =>
                    {
                        value.command_confirmation = config::CommandConfirmation::FullAccess;
                        confirmation = None;
                        config::save_config_result(&value)?;
                    }
                    config::CommandConfirmation::Always => {
                        confirmation = Some(PendingConfirmation::FullAccess);
                    }
                    config::CommandConfirmation::FullAccess => {
                        value.command_confirmation = config::CommandConfirmation::Smart;
                        confirmation = None;
                        config::save_config_result(&value)?;
                    }
                }
            }
            KeyCode::Char('e') if tab == 3 => return Ok(ExitAction::EditPersonalize),
            KeyCode::Char('d') if tab == 1 && !chats.is_empty() => {
                let id = chats[selected.min(chats.len() - 1)].id;
                if confirmation == Some(PendingConfirmation::Delete { tab, id }) {
                    memory::delete_memory_jobs_for_chat(&conn, id)?;
                    db::delete_chat(&conn, id)?;
                    confirmation = None;
                    selected = selected.saturating_sub(1);
                } else {
                    confirmation = Some(PendingConfirmation::Delete { tab, id });
                }
            }
            KeyCode::Char('d') if tab == 2 && !memories.is_empty() => {
                let id = memories[selected.min(memories.len() - 1)].0;
                if confirmation == Some(PendingConfirmation::Delete { tab, id }) {
                    memory::delete_memory(&conn, id)?;
                    confirmation = None;
                    selected = selected.saturating_sub(1);
                } else {
                    confirmation = Some(PendingConfirmation::Delete { tab, id });
                }
            }
            _ => {}
        }
    }
}

fn render_overview(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    cfg: &config::Config,
) {
    let model = if cfg.model.is_empty() {
        "not selected"
    } else {
        &cfg.model
    };
    let text = format!(
        "Gateway credential: {}\nModel: {}\nEmbedding model: {}\nAutomatic memory: {}\nTerminal context: {}\nCommand permissions: {}\n\nConfig: {}\nDatabase: {}\nPersonalization: {}",
        config::gateway_credential_source_for(cfg.gateway_provider),
        model,
        cfg.embedding_model,
        on_off(cfg.memory_enabled && cfg.auto_memory),
        on_off(cfg.terminal_context_enabled),
        cfg.command_confirmation.as_str(),
        config::get_config_path().display(),
        db::get_db_path().display(),
        personalize::path().display(),
    );
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::default().title(" Status ").borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn load_memories(conn: &Connection) -> Vec<(i64, String, bool)> {
    let Ok(mut statement) = conn.prepare(
        "SELECT id, text, pinned FROM memories WHERE superseded_by IS NULL AND (expires_at IS NULL OR expires_at > unixepoch()) ORDER BY pinned DESC, updated_at DESC LIMIT 200",
    ) else {
        return Vec::new();
    };
    statement
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? != 0))
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

fn on_off(value: bool) -> &'static str {
    if value {
        "on"
    } else {
        "off"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_cover_management_surfaces() {
        assert_eq!(TAB_NAMES, ["Overview", "Chats", "Memory", "Personalize"]);
    }
}
