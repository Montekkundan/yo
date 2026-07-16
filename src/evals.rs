//! File-based live workflow evals for Yo.
//!
//! Cases are TOML files under `evals/` whose names end in `.eval.toml`.
//! Each case runs the real configured Gateway model against the same assistant
//! tool loop as `yo ask`, but uses an in-memory chat database and an exact
//! command allowlist so eval runs cannot pollute normal chats or execute an
//! undeclared model-proposed command.

use crate::commands::{self, AgentTurnOutcome, CommandApproval, SessionContext, ToolTrace};
use crate::config::{self, Config};
use crate::db;
use crate::gateway::GatewayClient;
use crate::terminal;
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct EvalRunOptions {
    pub directory: PathBuf,
    pub filters: Vec<String>,
    pub list: bool,
    pub json: bool,
    pub verbose: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvalCaseFile {
    description: Option<String>,
    prompt: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    requires: Vec<String>,
    allowed_commands: Vec<String>,
    expect: EvalExpectation,
}

#[derive(Clone, Debug)]
struct EvalCase {
    id: String,
    path: PathBuf,
    definition: EvalCaseFile,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvalExpectation {
    #[serde(default = "default_tool")]
    tool: String,
    commands: Vec<String>,
    #[serde(default = "zero_exit_code")]
    exit_code: i32,
    #[serde(default)]
    stdout_nonempty: bool,
    #[serde(default)]
    stdout_contains: Vec<String>,
    #[serde(default = "default_true")]
    reply_nonempty: bool,
    #[serde(default)]
    reply_contains: Vec<String>,
}

fn default_tool() -> String {
    "run_command".to_owned()
}

fn zero_exit_code() -> i32 {
    0
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum EvalVerdict {
    Passed,
    Failed,
    Skipped,
}

#[derive(Clone, Debug, Serialize)]
struct AssertionResult {
    name: String,
    passed: bool,
    details: String,
}

#[derive(Clone, Debug, Serialize)]
struct EvalResult {
    id: String,
    description: Option<String>,
    tags: Vec<String>,
    verdict: EvalVerdict,
    duration_ms: u128,
    assertions: Vec<AssertionResult>,
    reply: Option<String>,
    tool_calls: Vec<ToolTrace>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct EvalSummary {
    model: String,
    passed: usize,
    failed: usize,
    skipped: usize,
    results: Vec<EvalResult>,
}

pub async fn run(options: EvalRunOptions) -> Result<()> {
    let all_cases = discover_cases(&options.directory)?;
    let cases = select_cases(all_cases, &options.filters)?;
    if options.list {
        print_case_list(&cases, options.json)?;
        return Ok(());
    }

    let mut eval_config = config::load_config()?;
    if eval_config.model.trim().is_empty() {
        anyhow::bail!("no model selected; run `yo setup` before live evals");
    }
    eval_config.memory_enabled = false;
    eval_config.auto_memory = false;
    eval_config.terminal_context_enabled = false;
    let gateway = GatewayClient::new(config::gateway_credential()?);

    let mut results = Vec::with_capacity(cases.len());
    for case in &cases {
        results.push(run_case(case, &gateway, &eval_config).await);
    }
    let summary = summarize(eval_config.model, results);
    if options.json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        print_human_summary(&summary, options.verbose);
    }
    if summary.failed > 0 {
        anyhow::bail!("{} eval(s) failed", summary.failed);
    }
    Ok(())
}

fn discover_cases(directory: &Path) -> Result<Vec<EvalCase>> {
    if !directory.is_dir() {
        anyhow::bail!(
            "eval directory {} does not exist; add *.eval.toml cases or pass --dir",
            directory.display()
        );
    }
    let mut paths = Vec::new();
    collect_case_paths(directory, &mut paths)?;
    paths.sort();
    let mut cases = Vec::with_capacity(paths.len());
    for path in paths {
        let source = fs::read_to_string(&path)
            .with_context(|| format!("failed to read eval {}", path.display()))?;
        let definition: EvalCaseFile =
            toml::from_str(&source).with_context(|| format!("invalid eval {}", path.display()))?;
        validate_case(&path, &definition)?;
        cases.push(EvalCase {
            id: case_id(directory, &path)?,
            path,
            definition,
        });
    }
    if cases.is_empty() {
        anyhow::bail!("no *.eval.toml cases found under {}", directory.display());
    }
    Ok(cases)
}

fn collect_case_paths(directory: &Path, paths: &mut Vec<PathBuf>) -> Result<()> {
    let entries = fs::read_dir(directory)
        .with_context(|| format!("failed to read eval directory {}", directory.display()))?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_case_paths(&path, paths)?;
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".eval.toml"))
        {
            paths.push(path);
        }
    }
    Ok(())
}

fn case_id(root: &Path, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(root)
        .with_context(|| format!("{} is outside {}", path.display(), root.display()))?;
    let mut id = relative
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    id.truncate(id.len().saturating_sub(".eval.toml".len()));
    Ok(id)
}

fn validate_case(path: &Path, case: &EvalCaseFile) -> Result<()> {
    if case.prompt.trim().is_empty() {
        anyhow::bail!("{} has an empty prompt", path.display());
    }
    if case.allowed_commands.is_empty() {
        anyhow::bail!("{} must declare allowed_commands", path.display());
    }
    if case.expect.commands.is_empty() {
        anyhow::bail!("{} must declare expect.commands", path.display());
    }
    if case.expect.commands.iter().any(|expected| {
        !case
            .allowed_commands
            .iter()
            .any(|allowed| allowed.trim() == expected.trim())
    }) {
        anyhow::bail!(
            "{} expects commands {:?}, but at least one is not in allowed_commands",
            path.display(),
            case.expect.commands
        );
    }
    if case.expect.tool != "run_command" {
        anyhow::bail!(
            "{} uses unsupported tool {:?}; Yo currently evaluates run_command workflows",
            path.display(),
            case.expect.tool
        );
    }
    for requirement in &case.requires {
        if requirement.is_empty()
            || !requirement.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            })
        {
            anyhow::bail!(
                "{} has invalid prerequisite command {:?}",
                path.display(),
                requirement
            );
        }
    }
    Ok(())
}

fn select_cases(cases: Vec<EvalCase>, filters: &[String]) -> Result<Vec<EvalCase>> {
    if filters.is_empty() {
        return Ok(cases);
    }
    let selected: Vec<_> = cases
        .into_iter()
        .filter(|case| {
            filters.iter().any(|filter| {
                let filter = filter.trim_matches('/');
                case.id == filter || case.id.starts_with(&format!("{filter}/"))
            })
        })
        .collect();
    if selected.is_empty() {
        anyhow::bail!("no evals matched: {}", filters.join(", "));
    }
    Ok(selected)
}

async fn run_case(case: &EvalCase, gateway: &GatewayClient, config: &Config) -> EvalResult {
    let started = Instant::now();
    if let Some(requirement) = missing_requirement(&case.definition.requires) {
        return EvalResult {
            id: case.id.clone(),
            description: case.definition.description.clone(),
            tags: case.definition.tags.clone(),
            verdict: EvalVerdict::Skipped,
            duration_ms: started.elapsed().as_millis(),
            assertions: Vec::new(),
            reply: None,
            tool_calls: Vec::new(),
            error: Some(format!("required command `{requirement}` is unavailable")),
        };
    }

    match execute_case(case, gateway, config).await {
        Ok(outcome) => grade_case(case, outcome, started.elapsed().as_millis()),
        Err(error) => EvalResult {
            id: case.id.clone(),
            description: case.definition.description.clone(),
            tags: case.definition.tags.clone(),
            verdict: EvalVerdict::Failed,
            duration_ms: started.elapsed().as_millis(),
            assertions: vec![assertion("run completed", false, error.to_string())],
            reply: None,
            tool_calls: Vec::new(),
            error: Some(format!("{error:#}")),
        },
    }
}

fn missing_requirement(requirements: &[String]) -> Option<String> {
    requirements.iter().find_map(|requirement| {
        let request = terminal::RunRequest::shell_script(format!("command -v {requirement}"))
            .ok()?
            .with_timeout(std::time::Duration::from_secs(10));
        match terminal::run_explicit(&request) {
            Ok(result) if result.success => None,
            _ => Some(requirement.clone()),
        }
    })
}

async fn execute_case(
    case: &EvalCase,
    gateway: &GatewayClient,
    config: &Config,
) -> Result<AgentTurnOutcome> {
    let conn = Connection::open_in_memory().context("failed to create isolated eval database")?;
    db::initialize_schema(&conn)?;
    let cwd = std::env::current_dir()?;
    let session_id = format!("yo-eval-{}", case.id.replace(['/', '\\'], "-"));
    let chat_id = db::ensure_session(
        &conn,
        &session_id,
        &std::env::var("SHELL").unwrap_or_else(|_| "unknown".into()),
        None,
        &cwd.display().to_string(),
        None,
    )?;
    db::insert_message(
        &conn,
        chat_id,
        "user",
        &terminal::redact_secrets(&case.definition.prompt),
        Some(&cwd.display().to_string()),
        None,
    )?;
    let session = SessionContext {
        id: session_id,
        chat_id,
        repo: None,
    };
    let messages = commands::build_chat_messages(&conn, config, &session, &[])?;
    commands::run_assistant_turn(
        gateway,
        &conn,
        &session,
        config,
        messages,
        &case.definition.prompt,
        false,
        CommandApproval::AllowOnly(&case.definition.allowed_commands),
    )
    .await
}

fn grade_case(case: &EvalCase, outcome: AgentTurnOutcome, duration_ms: u128) -> EvalResult {
    let expected = &case.definition.expect;
    let matching = outcome.tool_calls.iter().find(|trace| {
        trace.name == expected.tool
            && trace.arguments["command"].as_str().is_some_and(|command| {
                expected
                    .commands
                    .iter()
                    .any(|expected| command.trim() == expected.trim())
            })
    });
    let mut assertions = Vec::new();
    assertions.push(assertion(
        "called tool",
        matching.is_some(),
        format!(
            "expected {} with one of {:?}",
            expected.tool, expected.commands
        ),
    ));

    if let Some(trace) = matching {
        let completed = trace.output["ok"].as_bool() == Some(true);
        assertions.push(assertion(
            "tool completed",
            completed,
            trace.output.to_string(),
        ));
        let observed_exit = trace.output["exit_code"].as_i64();
        assertions.push(assertion(
            "exit code",
            observed_exit == Some(i64::from(expected.exit_code)),
            format!(
                "expected {}, observed {observed_exit:?}",
                expected.exit_code
            ),
        ));
        let stdout = trace.output["stdout"].as_str().unwrap_or_default();
        if expected.stdout_nonempty {
            assertions.push(assertion(
                "stdout is nonempty",
                !stdout.trim().is_empty(),
                preview(stdout),
            ));
        }
        for token in &expected.stdout_contains {
            assertions.push(assertion(
                &format!("stdout includes {token:?}"),
                stdout.contains(token),
                preview(stdout),
            ));
        }
    }

    if expected.reply_nonempty {
        assertions.push(assertion(
            "reply is nonempty",
            !outcome.reply.trim().is_empty(),
            preview(&outcome.reply),
        ));
    }
    for token in &expected.reply_contains {
        assertions.push(assertion(
            &format!("reply includes {token:?}"),
            outcome.reply.contains(token),
            preview(&outcome.reply),
        ));
    }
    let passed = assertions.iter().all(|assertion| assertion.passed);
    EvalResult {
        id: case.id.clone(),
        description: case.definition.description.clone(),
        tags: case.definition.tags.clone(),
        verdict: if passed {
            EvalVerdict::Passed
        } else {
            EvalVerdict::Failed
        },
        duration_ms,
        assertions,
        reply: Some(outcome.reply),
        tool_calls: outcome.tool_calls,
        error: None,
    }
}

fn assertion(name: &str, passed: bool, details: String) -> AssertionResult {
    AssertionResult {
        name: name.to_owned(),
        passed,
        details,
    }
}

fn preview(value: &str) -> String {
    const LIMIT: usize = 240;
    let safe = terminal::redact_secrets(value.trim());
    let mut preview: String = safe.chars().take(LIMIT).collect();
    if safe.chars().count() > LIMIT {
        preview.push('…');
    }
    preview
}

fn summarize(model: String, results: Vec<EvalResult>) -> EvalSummary {
    EvalSummary {
        model,
        passed: results
            .iter()
            .filter(|result| result.verdict == EvalVerdict::Passed)
            .count(),
        failed: results
            .iter()
            .filter(|result| result.verdict == EvalVerdict::Failed)
            .count(),
        skipped: results
            .iter()
            .filter(|result| result.verdict == EvalVerdict::Skipped)
            .count(),
        results,
    }
}

fn print_case_list(cases: &[EvalCase], json: bool) -> Result<()> {
    if json {
        let listed: Vec<_> = cases
            .iter()
            .map(|case| {
                serde_json::json!({
                    "id": case.id,
                    "description": case.definition.description,
                    "tags": case.definition.tags,
                    "path": case.path,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&listed)?);
    } else {
        for case in cases {
            println!(
                "{:<32} {}",
                case.id,
                case.definition.description.as_deref().unwrap_or("")
            );
        }
    }
    Ok(())
}

fn print_human_summary(summary: &EvalSummary, verbose: bool) {
    for result in &summary.results {
        let label = match result.verdict {
            EvalVerdict::Passed => "PASS",
            EvalVerdict::Failed => "FAIL",
            EvalVerdict::Skipped => "SKIP",
        };
        println!("{label} {} ({} ms)", result.id, result.duration_ms);
        for assertion in &result.assertions {
            let mark = if assertion.passed { "✓" } else { "✗" };
            println!("  {mark} {}", assertion.name);
            if verbose || !assertion.passed {
                println!("    {}", assertion.details);
            }
        }
        if let Some(error) = &result.error {
            println!("  {error}");
        }
        if verbose {
            if let Some(reply) = &result.reply {
                println!("  reply: {}", preview(reply));
            }
            for tool in &result.tool_calls {
                println!(
                    "  tool: {} {} -> {}",
                    tool.name,
                    tool.arguments,
                    preview(&tool.output.to_string())
                );
            }
        }
    }
    println!(
        "\n{} passed, {} failed, {} skipped (model: {})",
        summary.passed, summary.failed, summary.skipped, summary.model
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn case() -> EvalCase {
        EvalCase {
            id: "terminal/node-version".into(),
            path: "evals/terminal/node-version.eval.toml".into(),
            definition: EvalCaseFile {
                description: Some("node version".into()),
                prompt: "Run exactly `node --version`.".into(),
                tags: vec!["terminal".into()],
                requires: vec!["node".into()],
                allowed_commands: vec!["node --version".into()],
                expect: EvalExpectation {
                    tool: "run_command".into(),
                    commands: vec!["node --version".into()],
                    exit_code: 0,
                    stdout_nonempty: true,
                    stdout_contains: vec!["v20".into()],
                    reply_nonempty: true,
                    reply_contains: Vec::new(),
                },
            },
        }
    }

    #[test]
    fn grades_completed_command_tool_output() {
        let outcome = AgentTurnOutcome {
            reply: "Node v20 is installed.".into(),
            tool_calls: vec![ToolTrace {
                call_id: "call-1".into(),
                name: "run_command".into(),
                arguments: json!({"command": "node --version"}),
                output: json!({"ok": true, "exit_code": 0, "stdout": "v20.19.0\n"}),
            }],
        };
        let result = grade_case(&case(), outcome, 4);
        assert_eq!(result.verdict, EvalVerdict::Passed);
        assert!(result.assertions.iter().all(|assertion| assertion.passed));
    }

    #[test]
    fn denied_or_wrong_commands_fail_the_tool_gate() {
        let outcome = AgentTurnOutcome {
            reply: "I could not run it.".into(),
            tool_calls: vec![ToolTrace {
                call_id: "call-1".into(),
                name: "run_command".into(),
                arguments: json!({"command": "rm -rf /"}),
                output: json!({"ok": false, "denied": true}),
            }],
        };
        let result = grade_case(&case(), outcome, 4);
        assert_eq!(result.verdict, EvalVerdict::Failed);
        assert!(!result.assertions[0].passed);
    }

    #[test]
    fn ids_and_prefix_filters_follow_directory_layout() {
        let root = std::env::temp_dir().join(format!("yo-eval-discovery-{}", uuid::Uuid::new_v4()));
        let nested = root.join("terminal");
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            nested.join("node.eval.toml"),
            r#"
prompt = "Run exactly node --version"
allowed_commands = ["node --version"]

[expect]
commands = ["node --version"]
"#,
        )
        .unwrap();
        let cases = discover_cases(&root).unwrap();
        assert_eq!(cases[0].id, "terminal/node");
        assert_eq!(select_cases(cases, &["terminal".into()]).unwrap().len(), 1);
        let _ = fs::remove_dir_all(root);
    }
}
