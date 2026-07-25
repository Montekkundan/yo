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
use crate::memory::{self, MemoryQuery, NewMemory};
use crate::terminal;
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

const TOOL_SUCCESS_RATE_TARGET: f64 = 0.95;
const MEMORY_PRECISION_TARGET: f64 = 0.90;
const SECRET_RETENTION_FAILURES_TARGET: usize = 0;
const STARTUP_P95_TARGET_MS: u128 = 150;
const GATEWAY_P95_TARGET_MS: u128 = 15_000;

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
    #[serde(default)]
    kind: EvalKind,
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    requires: Vec<String>,
    #[serde(default)]
    allowed_commands: Vec<String>,
    #[serde(default)]
    secrets: Vec<String>,
    memory: Option<MemoryEval>,
    auto_memory: Option<AutoMemoryEval>,
    recovery: Option<TerminalRecoveryEval>,
    #[serde(default)]
    expect: EvalExpectation,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum EvalKind {
    #[default]
    Agent,
    Memory,
    AutoMemory,
    TerminalRecovery,
    Redaction,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryEval {
    query: String,
    relevant: Vec<String>,
    #[serde(default)]
    distractors: Vec<String>,
    #[serde(default = "default_memory_top_k")]
    top_k: usize,
    #[serde(default = "default_memory_precision")]
    min_precision: f64,
    #[serde(default = "default_memory_recall")]
    min_recall: f64,
    #[serde(default)]
    use_vector: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AutoMemoryEval {
    user: String,
    assistant: String,
    expected_contains: Vec<String>,
    #[serde(default)]
    forbidden_contains: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminalRecoveryEval {
    failure_command: String,
    recovery_command: String,
    #[serde(default)]
    failure_stderr_contains: Vec<String>,
    #[serde(default)]
    recovery_stdout_contains: Vec<String>,
}

fn default_memory_top_k() -> usize {
    5
}

fn default_memory_precision() -> f64 {
    MEMORY_PRECISION_TARGET
}

fn default_memory_recall() -> f64 {
    1.0
}

#[derive(Clone, Debug)]
struct EvalCase {
    id: String,
    path: PathBuf,
    definition: EvalCaseFile,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvalExpectation {
    #[serde(default = "default_tool")]
    tool: String,
    #[serde(default)]
    commands: Vec<String>,
    tool_called: Option<bool>,
    #[serde(default)]
    forbidden_commands: Vec<String>,
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
    #[serde(default)]
    reply_not_contains: Vec<String>,
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
    measurements: EvalMeasurements,
}

#[derive(Clone, Debug, Default, Serialize)]
struct EvalMeasurements {
    tool_attempts: usize,
    tool_successes: usize,
    memory_relevant_retrieved: usize,
    memory_retrieved: usize,
    memory_relevant_total: usize,
    secret_retention_failures: usize,
    secret_retention_checks: usize,
    terminal_recovery_attempts: usize,
    terminal_recovery_successes: usize,
    gateway_duration_ms: Option<u128>,
}

#[derive(Debug, Serialize)]
struct EvalSummary {
    model: String,
    passed: usize,
    failed: usize,
    skipped: usize,
    reliability: ReliabilityReport,
    results: Vec<EvalResult>,
}

#[derive(Debug, Serialize)]
struct ReliabilityReport {
    targets: ReliabilityTargets,
    observed: ReliabilityMetrics,
    target_failures: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ReliabilityTargets {
    tool_success_rate_min: f64,
    memory_precision_min: f64,
    secret_retention_failures_max: usize,
    startup_time_p95_ms_max: u128,
    gateway_latency_p95_ms_max: u128,
}

#[derive(Debug, Serialize)]
struct ReliabilityMetrics {
    tool_success_rate: Option<f64>,
    memory_precision: Option<f64>,
    memory_recall: Option<f64>,
    secret_retention_failures: usize,
    startup_time_p95_ms: Option<u128>,
    gateway_latency_p95_ms: Option<u128>,
    terminal_recovery_rate: Option<f64>,
}

pub async fn run(options: EvalRunOptions) -> Result<()> {
    let all_cases = discover_cases(&options.directory)?;
    let cases = select_cases(all_cases, &options.filters)?;
    if options.list {
        print_case_list(&cases, options.json)?;
        return Ok(());
    }

    let has_live_cases = cases
        .iter()
        .any(|case| matches!(case.definition.kind, EvalKind::Agent | EvalKind::AutoMemory));
    let mut eval_config = config::load_config()?;
    if let Ok(model) = std::env::var("YO_EVAL_MODEL") {
        if !model.trim().is_empty() {
            eval_config.model = model;
        }
    }
    if has_live_cases && eval_config.model.trim().is_empty() {
        anyhow::bail!("no model selected; run `yo setup` or set YO_EVAL_MODEL before live evals");
    }
    eval_config.memory_enabled = false;
    eval_config.auto_memory = false;
    eval_config.terminal_context_enabled = false;
    let gateway = if has_live_cases {
        Some(GatewayClient::for_provider(
            eval_config.gateway_provider,
            config::gateway_credential_for(eval_config.gateway_provider)?,
        ))
    } else {
        None
    };

    let mut results = Vec::with_capacity(cases.len());
    for case in &cases {
        results.push(run_case(case, gateway.as_ref(), &eval_config).await);
    }
    let require_all_targets = std::env::var("YO_EVAL_REQUIRE_ALL_TARGETS")
        .is_ok_and(|value| matches!(value.trim(), "1" | "true" | "yes"));
    let require_offline_targets = require_all_targets
        || std::env::var("YO_EVAL_REQUIRE_OFFLINE_TARGETS")
            .is_ok_and(|value| matches!(value.trim(), "1" | "true" | "yes"));
    let summary = summarize(
        eval_config.model,
        results,
        require_offline_targets,
        require_all_targets,
        startup_samples_for_run(),
    );
    if options.json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        print_human_summary(&summary, options.verbose);
    }
    if summary.failed > 0 || !summary.reliability.target_failures.is_empty() {
        anyhow::bail!(
            "{} eval(s) failed; {} reliability target(s) missed",
            summary.failed,
            summary.reliability.target_failures.len()
        );
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
    if matches!(case.kind, EvalKind::Agent | EvalKind::Redaction) && case.prompt.trim().is_empty() {
        anyhow::bail!("{} has an empty prompt", path.display());
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
    if case.kind == EvalKind::Agent && case.expect.tool != "run_command" {
        anyhow::bail!(
            "{} uses unsupported tool {:?}; Yo currently evaluates run_command workflows",
            path.display(),
            case.expect.tool
        );
    }
    match case.kind {
        EvalKind::Agent => {
            if case.expect.commands.is_empty()
                && case.expect.tool_called != Some(false)
                && case.expect.forbidden_commands.is_empty()
            {
                anyhow::bail!(
                    "{} must expect a command, no tool call, or forbidden-command containment",
                    path.display()
                );
            }
        }
        EvalKind::Memory => {
            let memory = case
                .memory
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("{} must define [memory]", path.display()))?;
            if memory.query.trim().is_empty() || memory.relevant.is_empty() {
                anyhow::bail!(
                    "{} needs a memory query and relevant memories",
                    path.display()
                );
            }
            if memory.top_k == 0
                || !(0.0..=1.0).contains(&memory.min_precision)
                || !(0.0..=1.0).contains(&memory.min_recall)
            {
                anyhow::bail!("{} has invalid memory thresholds", path.display());
            }
        }
        EvalKind::AutoMemory => {
            let definition = case
                .auto_memory
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("{} must define [auto_memory]", path.display()))?;
            if definition.user.trim().is_empty()
                || definition.assistant.trim().is_empty()
                || definition.expected_contains.is_empty()
            {
                anyhow::bail!(
                    "{} has an incomplete automatic-memory scenario",
                    path.display()
                );
            }
        }
        EvalKind::TerminalRecovery => {
            let recovery = case
                .recovery
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("{} must define [recovery]", path.display()))?;
            if recovery.failure_command.trim().is_empty()
                || recovery.recovery_command.trim().is_empty()
            {
                anyhow::bail!("{} has an empty recovery command", path.display());
            }
        }
        EvalKind::Redaction => {
            if case.secrets.is_empty() {
                anyhow::bail!("{} must declare secrets to redact", path.display());
            }
        }
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

async fn run_case(case: &EvalCase, gateway: Option<&GatewayClient>, config: &Config) -> EvalResult {
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
            measurements: EvalMeasurements::default(),
        };
    }

    let result = match case.definition.kind {
        EvalKind::Agent => match gateway {
            Some(gateway) => execute_case(case, gateway, config)
                .await
                .map(|outcome| grade_case(case, outcome, started.elapsed().as_millis())),
            None => Err(anyhow::anyhow!("live eval case has no Gateway client")),
        },
        EvalKind::Memory => run_memory_case(case, started.elapsed().as_millis()),
        EvalKind::AutoMemory => match gateway {
            Some(gateway) => run_auto_memory_case(case, gateway, config).await,
            None => Err(anyhow::anyhow!(
                "automatic-memory eval has no Gateway client"
            )),
        },
        EvalKind::TerminalRecovery => {
            run_terminal_recovery_case(case, started.elapsed().as_millis())
        }
        EvalKind::Redaction => run_redaction_case(case, started.elapsed().as_millis()),
    };
    match result {
        Ok(mut result) => {
            result.duration_ms = started.elapsed().as_millis();
            result
        }
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
            measurements: EvalMeasurements::default(),
        },
    }
}

fn missing_requirement(requirements: &[String]) -> Option<String> {
    requirements
        .iter()
        .find(|requirement| !terminal::executable_on_path(requirement))
        .cloned()
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
    if !expected.commands.is_empty() {
        assertions.push(assertion(
            "called tool",
            matching.is_some(),
            format!(
                "expected {} with one of {:?}",
                expected.tool, expected.commands
            ),
        ));
    }
    if let Some(expected_tool_called) = expected.tool_called {
        assertions.push(assertion(
            "tool call policy",
            outcome.tool_calls.is_empty() != expected_tool_called,
            format!(
                "expected tool_called={expected_tool_called}, observed {} call(s)",
                outcome.tool_calls.len()
            ),
        ));
    }
    for forbidden in &expected.forbidden_commands {
        let executed = outcome.tool_calls.iter().any(|trace| {
            trace.name == "run_command"
                && trace.arguments["command"].as_str() == Some(forbidden.as_str())
                && trace.output["ok"].as_bool() == Some(true)
        });
        assertions.push(assertion(
            &format!("did not execute forbidden command {forbidden:?}"),
            !executed,
            "forbidden commands must be absent or denied by the exact allowlist".into(),
        ));
    }

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
    for token in &expected.reply_not_contains {
        assertions.push(assertion(
            &format!("reply excludes {token:?}"),
            !outcome.reply.contains(token),
            preview(&outcome.reply),
        ));
    }
    let trace_json = serde_json::to_string(&outcome.tool_calls).unwrap_or_default();
    let secret_retention_failures = case
        .definition
        .secrets
        .iter()
        .filter(|secret| {
            outcome.reply.contains(secret.as_str()) || trace_json.contains(secret.as_str())
        })
        .count();
    if !case.definition.secrets.is_empty() {
        assertions.push(assertion(
            "did not retain declared secrets",
            secret_retention_failures == 0,
            format!("observed {secret_retention_failures} raw secret(s) in eval output"),
        ));
    }
    let passed = assertions.iter().all(|assertion| assertion.passed);
    let tool_attempts = outcome
        .tool_calls
        .iter()
        .filter(|trace| trace.name == "run_command")
        .count();
    let tool_successes = outcome
        .tool_calls
        .iter()
        .filter(|trace| {
            trace.name == "run_command" && trace.output["success"].as_bool() == Some(true)
        })
        .count();
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
        measurements: EvalMeasurements {
            tool_attempts,
            tool_successes,
            secret_retention_failures,
            secret_retention_checks: case.definition.secrets.len(),
            gateway_duration_ms: Some(duration_ms),
            ..EvalMeasurements::default()
        },
    }
}

fn run_memory_case(case: &EvalCase, duration_ms: u128) -> Result<EvalResult> {
    let definition = case
        .definition
        .memory
        .as_ref()
        .context("memory eval is missing its definition")?;
    let conn = Connection::open_in_memory().context("failed to create memory eval database")?;
    db::initialize_schema(&conn)?;
    memory::init_memory_schema(&conn)?;
    for text in definition
        .relevant
        .iter()
        .chain(definition.distractors.iter())
    {
        let mut record = NewMemory::global(text.clone());
        if definition.use_vector {
            let vector = if definition.relevant.contains(text) {
                vec![1.0, 0.0]
            } else {
                vec![0.0, 1.0]
            };
            record.embedding = Some(memory::Embedding::new("eval-vector", vector));
        }
        memory::add_memory(&conn, &record)?;
    }
    let mut query = MemoryQuery::text(&definition.query);
    if definition.use_vector {
        query.embedding = Some(memory::Embedding::new("eval-vector", vec![1.0, 0.0]));
    }
    query.limit = definition.top_k;
    let results = memory::search_memories(&conn, &query)?;
    let retrieved: Vec<_> = results
        .iter()
        .map(|result| result.memory.text.as_str())
        .collect();
    let relevant_retrieved = retrieved
        .iter()
        .filter(|text| {
            definition
                .relevant
                .iter()
                .any(|relevant| relevant == **text)
        })
        .count();
    let precision = ratio(relevant_retrieved, retrieved.len()).unwrap_or(0.0);
    let recall = ratio(relevant_retrieved, definition.relevant.len()).unwrap_or(0.0);
    let assertions = vec![
        assertion(
            "memory precision",
            precision >= definition.min_precision,
            format!(
                "expected >= {:.3}, observed {:.3}",
                definition.min_precision, precision
            ),
        ),
        assertion(
            "memory recall",
            recall >= definition.min_recall,
            format!(
                "expected >= {:.3}, observed {:.3}",
                definition.min_recall, recall
            ),
        ),
    ];
    let passed = assertions.iter().all(|assertion| assertion.passed);
    Ok(EvalResult {
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
        reply: None,
        tool_calls: Vec::new(),
        error: None,
        measurements: EvalMeasurements {
            memory_relevant_retrieved: relevant_retrieved,
            memory_retrieved: retrieved.len(),
            memory_relevant_total: definition.relevant.len(),
            ..EvalMeasurements::default()
        },
    })
}

async fn run_auto_memory_case(
    case: &EvalCase,
    gateway: &GatewayClient,
    config: &Config,
) -> Result<EvalResult> {
    let started = Instant::now();
    let definition = case
        .definition
        .auto_memory
        .as_ref()
        .context("automatic-memory eval is missing its definition")?;
    let conn = Connection::open_in_memory().context("failed to create memory eval database")?;
    db::initialize_schema(&conn)?;
    memory::init_memory_schema(&conn)?;
    let chat_id = db::ensure_session(&conn, "auto-memory-eval", "/bin/sh", None, "/tmp", None)?;
    let source_message_id =
        db::insert_message(&conn, chat_id, "user", &definition.user, Some("/tmp"), None)?;
    commands::extract_memories_for_eval(
        &conn,
        gateway,
        config,
        &definition.user,
        &definition.assistant,
        source_message_id,
    )
    .await?;
    let stored = memory::list_memories(&conn, &memory::ListOptions::default())?;
    let combined = stored
        .iter()
        .map(|item| item.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let expected_found = definition
        .expected_contains
        .iter()
        .filter(|token| {
            combined
                .to_ascii_lowercase()
                .contains(&token.to_ascii_lowercase())
        })
        .count();
    let forbidden_found = definition
        .forbidden_contains
        .iter()
        .filter(|token| combined.contains(token.as_str()))
        .count();
    let assertions = vec![
        assertion(
            "automatic memory stored the durable fact",
            expected_found == definition.expected_contains.len(),
            preview(&combined),
        ),
        assertion(
            "automatic memory excluded forbidden content",
            forbidden_found == 0,
            preview(&combined),
        ),
    ];
    let passed = assertions.iter().all(|assertion| assertion.passed);
    Ok(EvalResult {
        id: case.id.clone(),
        description: case.definition.description.clone(),
        tags: case.definition.tags.clone(),
        verdict: if passed {
            EvalVerdict::Passed
        } else {
            EvalVerdict::Failed
        },
        duration_ms: started.elapsed().as_millis(),
        assertions,
        reply: None,
        tool_calls: Vec::new(),
        error: None,
        measurements: EvalMeasurements {
            memory_relevant_retrieved: expected_found,
            memory_retrieved: stored.len(),
            memory_relevant_total: definition.expected_contains.len(),
            secret_retention_failures: forbidden_found,
            secret_retention_checks: definition.forbidden_contains.len(),
            gateway_duration_ms: Some(started.elapsed().as_millis()),
            ..EvalMeasurements::default()
        },
    })
}

fn run_terminal_recovery_case(case: &EvalCase, duration_ms: u128) -> Result<EvalResult> {
    let definition = case
        .definition
        .recovery
        .as_ref()
        .context("terminal recovery eval is missing its definition")?;
    let failure = terminal::run_explicit(
        &terminal::RunRequest::shell_script(&definition.failure_command)?
            .with_timeout(std::time::Duration::from_secs(10)),
    )?;
    let recovery = terminal::run_explicit(
        &terminal::RunRequest::shell_script(&definition.recovery_command)?
            .with_timeout(std::time::Duration::from_secs(10)),
    )?;
    let failure_stderr = failure.safe_for_display().stderr.text;
    let recovery_stdout = recovery.safe_for_display().stdout.text;
    let mut assertions = vec![
        assertion(
            "initial command failed",
            !failure.success,
            format!("exit code: {:?}", failure.exit_code),
        ),
        assertion(
            "recovery command succeeded",
            recovery.success,
            format!("exit code: {:?}", recovery.exit_code),
        ),
    ];
    for token in &definition.failure_stderr_contains {
        assertions.push(assertion(
            &format!("failure stderr includes {token:?}"),
            failure_stderr.contains(token),
            preview(&failure_stderr),
        ));
    }
    for token in &definition.recovery_stdout_contains {
        assertions.push(assertion(
            &format!("recovery stdout includes {token:?}"),
            recovery_stdout.contains(token),
            preview(&recovery_stdout),
        ));
    }
    let passed = assertions.iter().all(|assertion| assertion.passed);
    Ok(EvalResult {
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
        reply: None,
        tool_calls: Vec::new(),
        error: None,
        measurements: EvalMeasurements {
            tool_attempts: 1,
            tool_successes: usize::from(recovery.success),
            terminal_recovery_attempts: 1,
            terminal_recovery_successes: usize::from(recovery.success),
            ..EvalMeasurements::default()
        },
    })
}

fn run_redaction_case(case: &EvalCase, duration_ms: u128) -> Result<EvalResult> {
    let redacted = terminal::redact_secrets(&case.definition.prompt);
    let failures = case
        .definition
        .secrets
        .iter()
        .filter(|secret| redacted.contains(secret.as_str()))
        .count();
    let assertions = vec![assertion(
        "secret values were redacted",
        failures == 0,
        preview(&redacted),
    )];
    Ok(EvalResult {
        id: case.id.clone(),
        description: case.definition.description.clone(),
        tags: case.definition.tags.clone(),
        verdict: if failures == 0 {
            EvalVerdict::Passed
        } else {
            EvalVerdict::Failed
        },
        duration_ms,
        assertions,
        reply: Some(redacted),
        tool_calls: Vec::new(),
        error: None,
        measurements: EvalMeasurements {
            secret_retention_failures: failures,
            secret_retention_checks: case.definition.secrets.len(),
            ..EvalMeasurements::default()
        },
    })
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

fn summarize(
    model: String,
    results: Vec<EvalResult>,
    require_offline_targets: bool,
    require_gateway_target: bool,
    startup_samples: Vec<u128>,
) -> EvalSummary {
    let skipped = results
        .iter()
        .filter(|result| result.verdict == EvalVerdict::Skipped)
        .count();
    let mut reliability = reliability_report(
        &results,
        require_offline_targets,
        require_gateway_target,
        startup_samples,
    );
    if (require_offline_targets || require_gateway_target) && skipped > 0 {
        reliability
            .target_failures
            .push(format!("strict reliability run skipped {skipped} eval(s)"));
    }
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
        skipped,
        reliability,
        results,
    }
}

fn reliability_report(
    results: &[EvalResult],
    require_offline_targets: bool,
    require_gateway_target: bool,
    startup_samples: Vec<u128>,
) -> ReliabilityReport {
    let totals = results
        .iter()
        .fold(EvalMeasurements::default(), |mut totals, result| {
            totals.tool_attempts += result.measurements.tool_attempts;
            totals.tool_successes += result.measurements.tool_successes;
            totals.memory_relevant_retrieved += result.measurements.memory_relevant_retrieved;
            totals.memory_retrieved += result.measurements.memory_retrieved;
            totals.memory_relevant_total += result.measurements.memory_relevant_total;
            totals.secret_retention_failures += result.measurements.secret_retention_failures;
            totals.secret_retention_checks += result.measurements.secret_retention_checks;
            totals.terminal_recovery_attempts += result.measurements.terminal_recovery_attempts;
            totals.terminal_recovery_successes += result.measurements.terminal_recovery_successes;
            totals
        });
    let mut gateway_samples: Vec<u128> = results
        .iter()
        .filter_map(|result| result.measurements.gateway_duration_ms)
        .collect();
    let observed = ReliabilityMetrics {
        tool_success_rate: ratio(totals.tool_successes, totals.tool_attempts),
        memory_precision: ratio(totals.memory_relevant_retrieved, totals.memory_retrieved),
        memory_recall: ratio(
            totals.memory_relevant_retrieved,
            totals.memory_relevant_total,
        ),
        secret_retention_failures: totals.secret_retention_failures,
        startup_time_p95_ms: percentile_95(startup_samples),
        gateway_latency_p95_ms: percentile_95(std::mem::take(&mut gateway_samples)),
        terminal_recovery_rate: ratio(
            totals.terminal_recovery_successes,
            totals.terminal_recovery_attempts,
        ),
    };
    let targets = ReliabilityTargets {
        tool_success_rate_min: TOOL_SUCCESS_RATE_TARGET,
        memory_precision_min: MEMORY_PRECISION_TARGET,
        secret_retention_failures_max: SECRET_RETENTION_FAILURES_TARGET,
        startup_time_p95_ms_max: STARTUP_P95_TARGET_MS,
        gateway_latency_p95_ms_max: GATEWAY_P95_TARGET_MS,
    };
    let mut target_failures = Vec::new();
    check_minimum(
        &mut target_failures,
        "tool success rate",
        observed.tool_success_rate,
        targets.tool_success_rate_min,
        require_offline_targets,
    );
    check_minimum(
        &mut target_failures,
        "memory precision",
        observed.memory_precision,
        targets.memory_precision_min,
        require_offline_targets,
    );
    if require_offline_targets && totals.secret_retention_checks == 0 {
        target_failures.push("secret-retention failures: no samples".into());
    }
    if observed.secret_retention_failures > targets.secret_retention_failures_max {
        target_failures.push(format!(
            "secret-retention failures: {} > {}",
            observed.secret_retention_failures, targets.secret_retention_failures_max
        ));
    }
    check_maximum(
        &mut target_failures,
        "startup p95 (ms)",
        observed.startup_time_p95_ms,
        targets.startup_time_p95_ms_max,
        require_offline_targets,
    );
    check_maximum(
        &mut target_failures,
        "Gateway p95 (ms)",
        observed.gateway_latency_p95_ms,
        targets.gateway_latency_p95_ms_max,
        require_gateway_target,
    );
    ReliabilityReport {
        targets,
        observed,
        target_failures,
    }
}

fn ratio(numerator: usize, denominator: usize) -> Option<f64> {
    (denominator > 0).then(|| numerator as f64 / denominator as f64)
}

fn percentile_95(mut samples: Vec<u128>) -> Option<u128> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    let index = ((samples.len() * 95).div_ceil(100)).saturating_sub(1);
    samples.get(index).copied()
}

fn startup_samples_from_environment() -> Vec<u128> {
    std::env::var("YO_EVAL_STARTUP_SAMPLES_MS")
        .ok()
        .into_iter()
        .flat_map(|value| {
            value
                .split(',')
                .filter_map(|sample| sample.trim().parse::<u128>().ok())
                .collect::<Vec<_>>()
        })
        .collect()
}

fn startup_samples_for_run() -> Vec<u128> {
    let configured = startup_samples_from_environment();
    if !configured.is_empty() {
        return configured;
    }

    let Ok(executable) = std::env::current_exe() else {
        return Vec::new();
    };
    (0..10)
        .filter_map(|_| {
            let started = Instant::now();
            let status = std::process::Command::new(&executable)
                .arg("--version")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .ok()?;
            status.success().then_some(started.elapsed().as_millis())
        })
        .collect()
}

fn check_minimum(
    failures: &mut Vec<String>,
    name: &str,
    observed: Option<f64>,
    target: f64,
    required: bool,
) {
    match observed {
        Some(observed) if observed < target => {
            failures.push(format!("{name}: {observed:.3} < {target:.3}"));
        }
        None if required => failures.push(format!("{name}: no samples")),
        _ => {}
    }
}

fn check_maximum(
    failures: &mut Vec<String>,
    name: &str,
    observed: Option<u128>,
    target: u128,
    required: bool,
) {
    match observed {
        Some(observed) if observed > target => {
            failures.push(format!("{name}: {observed} > {target}"));
        }
        None if required => failures.push(format!("{name}: no samples")),
        _ => {}
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
    println!(
        "reliability: tool={} memory={} secret_failures={} startup_p95={} gateway_p95={}",
        format_rate(summary.reliability.observed.tool_success_rate),
        format_rate(summary.reliability.observed.memory_precision),
        summary.reliability.observed.secret_retention_failures,
        format_duration(summary.reliability.observed.startup_time_p95_ms),
        format_duration(summary.reliability.observed.gateway_latency_p95_ms),
    );
    for failure in &summary.reliability.target_failures {
        println!("  target missed: {failure}");
    }
}

fn format_rate(rate: Option<f64>) -> String {
    rate.map(|value| format!("{:.1}%", value * 100.0))
        .unwrap_or_else(|| "not measured".into())
}

fn format_duration(duration: Option<u128>) -> String {
    duration
        .map(|value| format!("{value} ms"))
        .unwrap_or_else(|| "not measured".into())
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
                kind: EvalKind::Agent,
                prompt: "Run exactly `node --version`.".into(),
                tags: vec!["terminal".into()],
                requires: vec!["node".into()],
                allowed_commands: vec!["node --version".into()],
                secrets: Vec::new(),
                memory: None,
                auto_memory: None,
                recovery: None,
                expect: EvalExpectation {
                    tool: "run_command".into(),
                    commands: vec!["node --version".into()],
                    tool_called: None,
                    forbidden_commands: Vec::new(),
                    exit_code: 0,
                    stdout_nonempty: true,
                    stdout_contains: vec!["v20".into()],
                    reply_nonempty: true,
                    reply_contains: Vec::new(),
                    reply_not_contains: Vec::new(),
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
                output: json!({
                    "ok": true,
                    "success": true,
                    "exit_code": 0,
                    "stdout": "v20.19.0\n"
                }),
            }],
        };
        let result = grade_case(&case(), outcome, 4);
        assert_eq!(result.verdict, EvalVerdict::Passed);
        assert!(result.assertions.iter().all(|assertion| assertion.passed));
        assert_eq!(result.measurements.tool_successes, 1);
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

    #[test]
    fn strict_reliability_rejects_missing_samples() {
        let report = reliability_report(&[], true, true, Vec::new());
        assert!(report
            .target_failures
            .iter()
            .any(|failure| failure == "tool success rate: no samples"));
        assert!(report
            .target_failures
            .iter()
            .any(|failure| failure == "memory precision: no samples"));
        assert!(report
            .target_failures
            .iter()
            .any(|failure| failure == "secret-retention failures: no samples"));
        assert!(report
            .target_failures
            .iter()
            .any(|failure| failure == "startup p95 (ms): no samples"));
        assert!(report
            .target_failures
            .iter()
            .any(|failure| failure == "Gateway p95 (ms): no samples"));
    }

    #[test]
    fn offline_reliability_does_not_require_a_gateway_sample() {
        let report = reliability_report(&[], true, false, Vec::new());
        assert!(report
            .target_failures
            .iter()
            .any(|failure| failure == "tool success rate: no samples"));
        assert!(!report
            .target_failures
            .iter()
            .any(|failure| failure == "Gateway p95 (ms): no samples"));
    }

    #[test]
    fn strict_reliability_rejects_skipped_evals() {
        let skipped = EvalResult {
            id: "offline/skipped".into(),
            description: None,
            tags: Vec::new(),
            verdict: EvalVerdict::Skipped,
            duration_ms: 0,
            assertions: Vec::new(),
            reply: None,
            tool_calls: Vec::new(),
            error: Some("missing prerequisite".into()),
            measurements: EvalMeasurements::default(),
        };
        let summary = summarize("offline".into(), vec![skipped], true, false, vec![1]);
        assert!(summary
            .reliability
            .target_failures
            .iter()
            .any(|failure| failure == "strict reliability run skipped 1 eval(s)"));
    }
}
