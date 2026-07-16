use crate::config::get_app_dir;
use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const DEFAULT_CONTENT: &str = "# How Yo should answer\n\n- Be concise and practical.\n- Prefer commands and concrete results when they help.\n";

pub fn path() -> PathBuf {
    get_app_dir().join("personalize.md")
}

pub fn ensure_exists() -> Result<PathBuf> {
    let path = path();
    if !path.exists() {
        write_private(&path, DEFAULT_CONTENT)?;
    }
    Ok(path)
}

pub fn load() -> Result<String> {
    let path = ensure_exists()?;
    fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))
}

/// Adds an ongoing user preference to `personalize.md`. This function is the
/// only write surface exposed to the model tool, so the model cannot choose an
/// arbitrary path or replace the whole file.
pub fn add_instruction(instruction: &str) -> Result<bool> {
    let instruction = instruction.trim().trim_start_matches(['-', '*']).trim();
    if instruction.is_empty() {
        anyhow::bail!("personalization instruction cannot be empty");
    }

    let path = ensure_exists()?;
    let mut contents = fs::read_to_string(&path)?;
    let normalized = instruction.to_lowercase();
    if contents
        .lines()
        .map(|line| {
            line.trim()
                .trim_start_matches(['-', '*'])
                .trim()
                .to_lowercase()
        })
        .any(|line| line == normalized)
    {
        return Ok(false);
    }
    if !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str("- ");
    contents.push_str(instruction);
    contents.push('\n');
    write_private(&path, &contents)?;
    Ok(true)
}

pub fn replace(contents: &str) -> Result<()> {
    let path = path();
    write_private(&path, contents)
}

pub fn reset() -> Result<()> {
    replace(DEFAULT_CONTENT)
}

pub fn edit() -> Result<()> {
    let path = ensure_exists()?;
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| {
            if cfg!(windows) {
                "notepad".into()
            } else {
                "vi".into()
            }
        });
    let status = Command::new(editor)
        .arg(&path)
        .status()
        .context("failed to open personalize.md in your editor")?;
    if !status.success() {
        anyhow::bail!("the editor exited with status {status}");
    }
    Ok(())
}

fn write_private(path: &PathBuf, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))?;
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

    #[test]
    fn default_file_is_markdown() {
        assert!(DEFAULT_CONTENT.starts_with("# How Yo should answer"));
        assert!(DEFAULT_CONTENT.contains("- Be concise"));
    }

    #[test]
    fn instructions_are_sanitized_to_one_list_item() {
        let value = "- be more casual"
            .trim()
            .trim_start_matches(['-', '*'])
            .trim();
        assert_eq!(value, "be more casual");
    }
}
