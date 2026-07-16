use crate::terminal;
use crossterm::cursor::MoveToColumn;
use crossterm::terminal::{Clear, ClearType};
use crossterm::QueueableCommand;
use std::io::{self, IsTerminal, Write};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

enum ProgressEvent {
    Update(String),
    Stop,
}

/// A single-line spinner that is silent when stderr is redirected.
///
/// Progress belongs on stderr so normal answers and `--json` output remain
/// clean and pipeable. The worker owns all terminal drawing, avoiding mixed
/// writes while command output arrives from multiple capture threads.
pub struct ProgressLine {
    enabled: bool,
    sender: Option<Sender<ProgressEvent>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl ProgressLine {
    pub fn new(message: impl Into<String>) -> Self {
        let enabled = progress_enabled();
        if !enabled {
            return Self {
                enabled,
                sender: None,
                worker: None,
            };
        }
        let (sender, receiver) = mpsc::channel();
        let mut message = sanitize_status(&message.into());
        let worker = thread::spawn(move || {
            const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let mut frame = 0_usize;
            loop {
                draw_progress(FRAMES[frame % FRAMES.len()], &message);
                frame += 1;
                match receiver.recv_timeout(Duration::from_millis(80)) {
                    Ok(ProgressEvent::Update(updated)) => message = sanitize_status(&updated),
                    Ok(ProgressEvent::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                        clear_progress();
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
        });
        Self {
            enabled,
            sender: Some(sender),
            worker: Some(worker),
        }
    }

    pub fn update(&self, message: impl Into<String>) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(ProgressEvent::Update(message.into()));
        }
    }

    pub fn clear(mut self) {
        self.stop();
    }

    pub fn fail(mut self, message: impl Into<String>) {
        self.stop();
        if self.enabled {
            eprintln!("Error: {}", sanitize_status(&message.into()));
        }
    }

    fn stop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(ProgressEvent::Stop);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for ProgressLine {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn status_excerpt(output: &str) -> String {
    let line = output
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    sanitize_status(&terminal::redact_secrets(line))
}

fn progress_enabled() -> bool {
    io::stderr().is_terminal()
        && std::env::var("TERM").map_or(true, |value| value != "dumb")
        && std::env::var_os("YO_NO_SPINNER").is_none()
}

fn draw_progress(frame: &str, message: &str) {
    let width = crossterm::terminal::size()
        .map(|(columns, _)| usize::from(columns))
        .unwrap_or(80);
    let available = width.saturating_sub(frame.chars().count() + 2);
    let message: String = message.chars().take(available).collect();
    let mut stderr = io::stderr();
    let _ = stderr
        .queue(MoveToColumn(0))
        .and_then(|stream| stream.queue(Clear(ClearType::CurrentLine)))
        .and_then(|stream| write!(stream, "{frame} {message}").map(|_| stream))
        .and_then(|stream| stream.flush());
}

fn clear_progress() {
    let mut stderr = io::stderr();
    let _ = stderr
        .queue(MoveToColumn(0))
        .and_then(|stream| stream.queue(Clear(ClearType::CurrentLine)))
        .and_then(|stream| stream.flush());
}

fn sanitize_status(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut escape = false;
    let mut previous_space = false;
    for character in input.chars() {
        if escape {
            if character.is_ascii_alphabetic() || character == '~' {
                escape = false;
            }
            continue;
        }
        if character == '\u{1b}' {
            escape = true;
            continue;
        }
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if character.is_whitespace() {
            if !previous_space {
                output.push(' ');
            }
            previous_space = true;
        } else {
            output.push(character);
            previous_space = false;
        }
    }
    output.trim().to_owned()
}

/// Render model Markdown like a small built-in Glow. Interactive terminals get
/// wrapping and ANSI styling; redirected output stays as portable Markdown.
pub fn markdown(markdown: &str) {
    if io::stdout().is_terminal() {
        if colors_enabled() {
            let skin = terminal_skin();
            println!("{}", skin.term_text(markdown.trim_end()));
        } else {
            println!("{}", plain_text(markdown));
        }
    } else {
        print!("{}", markdown);
        if !markdown.ends_with('\n') {
            println!();
        }
    }
    let _ = io::stdout().flush();
}

fn terminal_skin() -> termimad::MadSkin {
    use crossterm::style::Attribute;
    use termimad::{Alignment, CompoundStyle, MadSkin};

    let mut skin = MadSkin::no_style();
    skin.bold = CompoundStyle::with_attr(Attribute::Bold);
    skin.italic = CompoundStyle::with_attr(Attribute::Italic);
    for header in &mut skin.headers {
        header.compound_style = CompoundStyle::with_attr(Attribute::Bold);
        header.align = Alignment::Left;
    }
    skin
}

fn colors_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none()
        && std::env::var("CLICOLOR").map_or(true, |value| value != "0")
        && std::env::var("TERM").map_or(true, |value| value != "dumb")
}

pub fn plain_text(markdown: &str) -> String {
    let mut in_fence = false;
    let mut output = String::with_capacity(markdown.len());
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            output.push_str("    ");
            output.push_str(line);
        } else {
            let without_heading = trimmed.trim_start_matches('#').trim_start();
            output.push_str(&strip_inline_markers(without_heading));
        }
        output.push('\n');
    }
    output.trim_end().to_owned()
}

fn strip_inline_markers(line: &str) -> String {
    line.replace("**", "").replace("__", "").replace('`', "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_render_removes_fence_markers_but_keeps_code() {
        let rendered = plain_text("Answer:\n```sh\nnvm list\n```\n**Done**");
        assert!(!rendered.contains("```"));
        assert!(rendered.contains("    nvm list"));
        assert!(rendered.contains("Done"));
    }

    #[test]
    fn plain_render_removes_heading_marker() {
        assert_eq!(plain_text("## Result"), "Result");
    }

    #[test]
    fn status_excerpt_uses_last_line_and_removes_control_sequences() {
        let rendered = status_excerpt("first\n\u{1b}[32mPython 3.14.6\u{1b}[0m\n");
        assert_eq!(rendered, "Python 3.14.6");
    }

    #[test]
    fn terminal_skin_does_not_highlight_code() {
        let skin = terminal_skin();
        assert_eq!(skin.inline_code.object_style.background_color, None);
        assert_eq!(
            skin.code_block.compound_style.object_style.background_color,
            None
        );
    }
}
