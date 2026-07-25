//! Terminal session identity, explicit command execution, and safe output display.
//!
//! The execution functions in this module deliberately do not enforce the
//! advisory command classification. A future `yo run -- ...` command can call
//! [`run_cli_args`] directly and must not silently block an explicit user
//! request. Model-initiated execution can use [`classify_command`] first and
//! require confirmation when appropriate.

use crate::sandbox::{self, CommandSpec, SandboxPolicy};
use std::collections::VecDeque;
use std::env;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const SESSION_ID_ENV: &str = "YO_SESSION_ID";
pub const DEFAULT_CAPTURE_LIMIT_BYTES: usize = 64 * 1024;

static FALLBACK_SESSION_ID: OnceLock<String> = OnceLock::new();

/// Return the shell-provided session ID, or a stable process-local fallback.
///
/// Installing one of [`shell_init_snippet`]'s snippets makes the ID stable
/// across separate `yo` invocations in the same terminal shell. Without the
/// environment variable, the fallback can only be stable for this process.
pub fn current_session_id() -> String {
    if let Ok(value) = env::var(SESSION_ID_ENV) {
        let value = normalize_session_id(&value);
        if !value.is_empty() {
            return value;
        }
    }

    FALLBACK_SESSION_ID
        .get_or_init(generate_fallback_session_id)
        .clone()
}

fn normalize_session_id(value: &str) -> String {
    value
        .trim()
        .chars()
        .take(128)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn generate_fallback_session_id() -> String {
    if let Some(source) = terminal_session_source() {
        return format!("yo-auto-{:016x}", stable_hash(source.as_bytes()));
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("yo-{}-{nanos:x}", std::process::id())
}

/// Best-effort identity shared by separate `yo` processes launched from the
/// same terminal. Explicit shell initialization remains the strongest path.
fn terminal_session_source() -> Option<String> {
    const TERMINAL_ENV_HINTS: &[&str] = &[
        "TERM_SESSION_ID",
        "ITERM_SESSION_ID",
        "WEZTERM_PANE",
        "KITTY_WINDOW_ID",
        "TMUX_PANE",
        "ZELLIJ_PANE_ID",
        "WT_SESSION",
        "SSH_TTY",
    ];

    for key in TERMINAL_ENV_HINTS {
        if let Ok(value) = env::var(key) {
            if !value.trim().is_empty() {
                return Some(format!("env:{key}={}", value.trim()));
            }
        }
    }

    #[cfg(unix)]
    if let Some(tty) = controlling_tty() {
        return Some(format!("tty:{tty}"));
    }

    #[cfg(unix)]
    if let Some(parent_pid) = parent_process_id() {
        return Some(format!("ppid:{parent_pid}"));
    }

    None
}

#[cfg(unix)]
fn controlling_tty() -> Option<String> {
    let output = Command::new("tty")
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let tty = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!tty.is_empty()).then_some(tty)
}

#[cfg(unix)]
fn parent_process_id() -> Option<u32> {
    let output = Command::new("ps")
        .args(["-o", "ppid=", "-p"])
        .arg(std::process::id().to_string())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

fn stable_hash(bytes: &[u8]) -> u64 {
    // FNV-1a is tiny, deterministic across processes, and sufficient for a
    // non-security session identifier.
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellKind {
    Bash,
    Zsh,
    Fish,
    Posix,
    Cmd,
}

impl ShellKind {
    pub fn from_path(path: impl AsRef<Path>) -> Self {
        let name = path
            .as_ref()
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();

        match name.as_str() {
            "bash" => Self::Bash,
            "zsh" => Self::Zsh,
            "fish" => Self::Fish,
            "cmd" | "cmd.exe" => Self::Cmd,
            _ => Self::Posix,
        }
    }
}

pub fn current_shell_kind() -> ShellKind {
    env::var_os("SHELL")
        .or_else(|| env::var_os("COMSPEC"))
        .map(ShellKind::from_path)
        .unwrap_or(ShellKind::Posix)
}

/// Generate an idempotent shell-init snippet that assigns one ID per shell.
///
/// The private owner-PID variable is intentionally not exported. A child shell
/// therefore creates its own ID, while re-sourcing the snippet in the current
/// shell keeps the existing ID.
pub fn shell_init_snippet(shell: ShellKind) -> &'static str {
    match shell {
        ShellKind::Fish => {
            r#"# yo: keep one conversation ID per shell
if not set -q __yo_session_owner_pid; or test "$__yo_session_owner_pid" != "$fish_pid"
    set -g __yo_session_owner_pid $fish_pid
    set -gx YO_SESSION_ID "yo-"(date +%s)"-"$fish_pid"-"(random)
end

# yo: expose the previous command and status to the next `yo` invocation
if not functions -q __yo_postexec
    function __yo_postexec --on-event fish_postexec
        set -gx YO_LAST_EXIT_CODE $status
        set -gx YO_LAST_COMMAND $argv
        set -gx YO_LAST_CWD $PWD
    end
end"#
        }
        ShellKind::Cmd => {
            r#"@rem yo: create a conversation ID for this cmd.exe session
@for /f "tokens=*" %%i in ('powershell -NoProfile -Command "[guid]::NewGuid().ToString('N')"') do @set "YO_SESSION_ID=yo-%%i""#
        }
        ShellKind::Bash => {
            r#"# yo: keep one conversation ID per shell
if [ "${__YO_SESSION_OWNER_PID:-}" != "$$" ]; then
  __YO_SESSION_OWNER_PID=$$
  export YO_SESSION_ID="yo-$(date +%s)-$$-${RANDOM:-0}"
fi

# yo: expose the previous command and status to the next `yo` invocation
__yo_prompt_context() {
  local __yo_status=$?
  export YO_LAST_EXIT_CODE="$__yo_status"
  export YO_LAST_COMMAND="$(HISTTIMEFORMAT= builtin history 1 | sed 's/^[[:space:]]*[0-9]\+[[:space:]]*//')"
  export YO_LAST_CWD="$PWD"
  return "$__yo_status"
}
case ";${PROMPT_COMMAND:-};" in
  *";__yo_prompt_context;"*) ;;
  *) PROMPT_COMMAND="__yo_prompt_context${PROMPT_COMMAND:+;$PROMPT_COMMAND}" ;;
esac"#
        }
        ShellKind::Zsh => {
            r#"# yo: keep one conversation ID per shell
if [ "${__YO_SESSION_OWNER_PID:-}" != "$$" ]; then
  __YO_SESSION_OWNER_PID=$$
  export YO_SESSION_ID="yo-$(date +%s)-$$-${RANDOM:-0}"
fi

# yo: expose the previous command and status to the next `yo` invocation
autoload -Uz add-zsh-hook
__yo_prompt_context() {
  local __yo_status=$?
  export YO_LAST_EXIT_CODE="$__yo_status"
  export YO_LAST_COMMAND="$(fc -ln -1 2>/dev/null | sed 's/^[[:space:]]*//')"
  export YO_LAST_CWD="$PWD"
}
if (( ${precmd_functions[(I)__yo_prompt_context]} == 0 )); then
  add-zsh-hook precmd __yo_prompt_context
fi"#
        }
        ShellKind::Posix => {
            r#"# yo: keep one conversation ID per shell
if [ "${__YO_SESSION_OWNER_PID:-}" != "$$" ]; then
  __YO_SESSION_OWNER_PID=$$
  export YO_SESSION_ID="yo-$(date +%s)-$$-${RANDOM:-0}"
fi"#
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShellIntegrationInstall {
    pub path: Option<PathBuf>,
    pub added: bool,
    pub activation_command: Option<&'static str>,
}

/// Add Yo's session/context hook to the detected shell startup file once.
pub fn install_shell_integration(shell: ShellKind) -> io::Result<ShellIntegrationInstall> {
    let Some((path, command)) = shell_integration_target(shell) else {
        return Ok(ShellIntegrationInstall {
            path: None,
            added: false,
            activation_command: None,
        });
    };
    let added = install_shell_integration_at(&path, command)?;
    Ok(ShellIntegrationInstall {
        path: Some(path),
        added,
        activation_command: Some(command),
    })
}

fn shell_integration_target(shell: ShellKind) -> Option<(PathBuf, &'static str)> {
    let home = dirs::home_dir()?;
    match shell {
        ShellKind::Zsh => {
            let directory = env::var_os("ZDOTDIR").map(PathBuf::from).unwrap_or(home);
            Some((directory.join(".zshrc"), r#"eval "$(yo init zsh)""#))
        }
        ShellKind::Bash => Some((home.join(".bashrc"), r#"eval "$(yo init bash)""#)),
        ShellKind::Fish => {
            let config = env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config"));
            Some((config.join("fish/config.fish"), "yo init fish | source"))
        }
        ShellKind::Posix | ShellKind::Cmd => None,
    }
}

fn install_shell_integration_at(path: &Path, command: &str) -> io::Result<bool> {
    let existing = match fs::read_to_string(path) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    if existing.contains("# >>> yo setup >>>") || existing.contains(command) {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(file)?;
    }
    writeln!(file, "# >>> yo setup >>>")?;
    writeln!(file, "{command}")?;
    writeln!(file, "# <<< yo setup <<<")?;
    Ok(true)
}

#[derive(Clone, Eq, PartialEq)]
pub enum RunInput {
    /// An executable plus arguments. Arguments are passed through the shell as
    /// positional parameters, so they are not re-parsed as shell syntax.
    Argv(Vec<String>),
    /// An explicitly supplied shell program, including any pipes or redirects.
    Script(String),
}

impl fmt::Debug for RunInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&format_run_input(self))
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct RunRequest {
    pub input: RunInput,
    pub cwd: Option<PathBuf>,
    pub shell: Option<PathBuf>,
    pub capture_limit_bytes: usize,
    pub timeout: Option<Duration>,
    pub sandbox: Option<SandboxPolicy>,
}

impl fmt::Debug for RunRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RunRequest")
            .field("input", &format_run_input(&self.input))
            .field("cwd", &self.cwd)
            .field("shell", &self.shell)
            .field("capture_limit_bytes", &self.capture_limit_bytes)
            .field("timeout", &self.timeout)
            .field("sandbox", &self.sandbox.as_ref().map(|policy| policy.mode))
            .finish()
    }
}

impl RunRequest {
    /// Build a request from trailing CLI arguments, accepting either the args
    /// after clap has stripped `--` or an input slice that still starts with it.
    pub fn from_cli_args<I, S>(arguments: I) -> io::Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut arguments: Vec<String> = arguments.into_iter().map(Into::into).collect();
        if arguments.first().is_some_and(|argument| argument == "--") {
            arguments.remove(0);
        }
        if arguments.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "yo run requires a command after --",
            ));
        }

        Ok(Self {
            input: RunInput::Argv(arguments),
            cwd: None,
            shell: None,
            capture_limit_bytes: DEFAULT_CAPTURE_LIMIT_BYTES,
            timeout: None,
            sandbox: None,
        })
    }

    pub fn shell_script(script: impl Into<String>) -> io::Result<Self> {
        let script = script.into();
        if script.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "shell script cannot be empty",
            ));
        }

        Ok(Self {
            input: RunInput::Script(script),
            cwd: None,
            shell: None,
            capture_limit_bytes: DEFAULT_CAPTURE_LIMIT_BYTES,
            timeout: None,
            sandbox: None,
        })
    }

    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_shell(mut self, shell: impl Into<PathBuf>) -> Self {
        self.shell = Some(shell.into());
        self
    }

    pub fn with_capture_limit(mut self, bytes_per_stream: usize) -> Self {
        self.capture_limit_bytes = bytes_per_stream;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn with_sandbox(mut self, policy: SandboxPolicy) -> Self {
        self.sandbox = Some(policy);
        self
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct CapturedStream {
    pub text: String,
    pub truncated: bool,
    pub omitted_bytes: usize,
}

impl fmt::Debug for CapturedStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapturedStream")
            .field("bytes", &self.text.len())
            .field("truncated", &self.truncated)
            .field("omitted_bytes", &self.omitted_bytes)
            .finish()
    }
}

pub struct CommandResult {
    pub session_id: String,
    pub input: RunInput,
    pub shell: PathBuf,
    pub cwd: PathBuf,
    pub stdout: CapturedStream,
    pub stderr: CapturedStream,
    pub exit_code: Option<i32>,
    pub success: bool,
    pub timed_out: bool,
    pub duration: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

struct OutputChunk {
    stream: OutputStream,
    bytes: Vec<u8>,
}

impl fmt::Debug for CommandResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.safe_for_display().fmt(formatter)
    }
}

impl CommandResult {
    /// Produce a value that is safe to render in a terminal or send to a model.
    /// The raw result intentionally has no `Display` implementation.
    pub fn safe_for_display(&self) -> SafeCommandDisplay {
        SafeCommandDisplay {
            session_id: self.session_id.clone(),
            command: format_run_input(&self.input),
            shell: redact_secrets(&self.shell.to_string_lossy()),
            cwd: redact_secrets(&self.cwd.to_string_lossy()),
            stdout: SafeCapturedStream {
                text: redact_secrets(&self.stdout.text),
                truncated: self.stdout.truncated,
                omitted_bytes: self.stdout.omitted_bytes,
            },
            stderr: SafeCapturedStream {
                text: redact_secrets(&self.stderr.text),
                truncated: self.stderr.truncated,
                omitted_bytes: self.stderr.omitted_bytes,
            },
            exit_code: self.exit_code,
            success: self.success,
            timed_out: self.timed_out,
            duration: self.duration,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SafeCapturedStream {
    pub text: String,
    pub truncated: bool,
    pub omitted_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SafeCommandDisplay {
    pub session_id: String,
    pub command: String,
    pub shell: String,
    pub cwd: String,
    pub stdout: SafeCapturedStream,
    pub stderr: SafeCapturedStream,
    pub exit_code: Option<i32>,
    pub success: bool,
    pub timed_out: bool,
    pub duration: Duration,
}

impl fmt::Display for SafeCommandDisplay {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "$ {}", self.command)?;
        writeln!(formatter, "cwd: {}", self.cwd)?;
        let exit = if self.timed_out {
            "timeout".to_string()
        } else {
            self.exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_string())
        };
        writeln!(formatter, "exit: {exit} ({} ms)", self.duration.as_millis())?;
        if !self.stdout.text.is_empty() {
            writeln!(formatter, "stdout:")?;
            write!(formatter, "{}", self.stdout.text)?;
            if !self.stdout.text.ends_with('\n') {
                writeln!(formatter)?;
            }
        }
        if !self.stderr.text.is_empty() {
            writeln!(formatter, "stderr:")?;
            write!(formatter, "{}", self.stderr.text)?;
            if !self.stderr.text.ends_with('\n') {
                writeln!(formatter)?;
            }
        }
        Ok(())
    }
}

/// Execute explicitly supplied trailing CLI arguments.
///
/// This is the convenience API intended for a future clap `yo run -- ...`
/// branch. It never applies command-risk policy or asks for confirmation.
pub fn run_cli_args(arguments: &[String]) -> io::Result<CommandResult> {
    let request = RunRequest::from_cli_args(arguments.iter().cloned())?;
    run_explicit(&request)
}

/// Execute an explicit request through the selected user shell.
///
/// This function always executes the request. Callers handling model-proposed
/// commands should classify first; callers handling explicit `yo run` input
/// should not silently turn the advisory classification into a block.
pub fn run_explicit(request: &RunRequest) -> io::Result<CommandResult> {
    run_explicit_with_progress(request, |_, _| {})
}

/// Execute a command while reporting stdout/stderr chunks as they arrive.
///
/// The command receives closed stdin. Yo captures non-interactive commands;
/// inheriting the approval prompt's terminal can leave interactive login-shell
/// hooks waiting forever after the user presses `y`.
pub fn run_explicit_with_progress<F>(
    request: &RunRequest,
    mut on_output: F,
) -> io::Result<CommandResult>
where
    F: FnMut(OutputStream, &str),
{
    validate_run_input(&request.input)?;

    let shell = request.shell.clone().unwrap_or_else(user_shell);
    let cwd = resolve_cwd(request.cwd.as_deref())?;
    let mut command = command_for_request(&shell, &request.input);
    command.current_dir(&cwd);
    if let Some(policy) = &request.sandbox {
        let spec = CommandSpec::from_command(&command, &cwd);
        command = sandbox::prepare(&spec, policy)
            .map_err(|error| io::Error::other(error.to_string()))?
            .into_command();
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let started = Instant::now();
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("failed to capture command stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("failed to capture command stderr"))?;
    let limit = request.capture_limit_bytes;
    let (sender, receiver) = mpsc::channel();
    let stdout_sender = sender.clone();
    let stdout_reader = thread::spawn(move || {
        read_bounded_tail_reporting(stdout, limit, |bytes| {
            let _ = stdout_sender.send(OutputChunk {
                stream: OutputStream::Stdout,
                bytes: bytes.to_vec(),
            });
        })
    });
    let stderr_reader = thread::spawn(move || {
        read_bounded_tail_reporting(stderr, limit, |bytes| {
            let _ = sender.send(OutputChunk {
                stream: OutputStream::Stderr,
                bytes: bytes.to_vec(),
            });
        })
    });

    let status_result =
        wait_for_child_with_output(&mut child, request.timeout, &receiver, &mut on_output);
    let stdout = join_reader(stdout_reader, "stdout")?;
    let stderr = join_reader(stderr_reader, "stderr")?;
    while let Ok(chunk) = receiver.try_recv() {
        report_chunk(chunk, &mut on_output);
    }
    let (status, timed_out) = status_result?;

    Ok(CommandResult {
        session_id: current_session_id(),
        input: request.input.clone(),
        shell,
        cwd,
        stdout,
        stderr,
        exit_code: status.code(),
        success: status.success(),
        timed_out,
        duration: started.elapsed(),
    })
}

fn wait_for_child_with_output<F>(
    child: &mut std::process::Child,
    timeout: Option<Duration>,
    receiver: &mpsc::Receiver<OutputChunk>,
    on_output: &mut F,
) -> io::Result<(std::process::ExitStatus, bool)>
where
    F: FnMut(OutputStream, &str),
{
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            #[cfg(unix)]
            terminate_lingering_process_group(child.id());
            return Ok((status, false));
        }
        if timeout.is_some_and(|timeout| started.elapsed() >= timeout) {
            terminate_timed_out_child(child);
            return child.wait().map(|status| (status, true));
        }
        match receiver.recv_timeout(Duration::from_millis(25)) {
            Ok(chunk) => report_chunk(chunk, on_output),
            Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {}
        }
    }
}

fn report_chunk<F>(chunk: OutputChunk, on_output: &mut F)
where
    F: FnMut(OutputStream, &str),
{
    on_output(chunk.stream, &String::from_utf8_lossy(&chunk.bytes));
}

#[cfg(unix)]
fn terminate_lingering_process_group(process_id: u32) {
    let process_group = format!("-{process_id}");
    let terminated = Command::new("kill")
        .args(["-TERM", "--", &process_group])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if terminated {
        thread::sleep(Duration::from_millis(25));
        let _ = Command::new("kill")
            .args(["-KILL", "--", &process_group])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(unix)]
fn terminate_timed_out_child(child: &mut std::process::Child) {
    let process_group = format!("-{}", child.id());
    let _ = Command::new("kill")
        .args(["-TERM", "--", &process_group])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let grace_started = Instant::now();
    while grace_started.elapsed() < Duration::from_millis(500) {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let _ = Command::new("kill")
        .args(["-KILL", "--", &process_group])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
}

#[cfg(not(unix))]
fn terminate_timed_out_child(child: &mut std::process::Child) {
    let _ = child.kill();
}

fn validate_run_input(input: &RunInput) -> io::Result<()> {
    let valid = match input {
        RunInput::Argv(arguments) => !arguments.is_empty() && !arguments[0].is_empty(),
        RunInput::Script(script) => !script.trim().is_empty(),
    };
    if valid {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "command cannot be empty",
        ))
    }
}

fn user_shell() -> PathBuf {
    if let Ok(shell) = env::var("SHELL") {
        if !shell.trim().is_empty() {
            return PathBuf::from(shell);
        }
    }
    if let Ok(shell) = env::var("COMSPEC") {
        if !shell.trim().is_empty() {
            return PathBuf::from(shell);
        }
    }

    if cfg!(windows) {
        PathBuf::from("cmd.exe")
    } else {
        PathBuf::from("/bin/sh")
    }
}

fn resolve_cwd(requested: Option<&Path>) -> io::Result<PathBuf> {
    let current = env::current_dir()?;
    let path = match requested {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => current.join(path),
        None => current,
    };
    fs::canonicalize(path)
}

fn command_for_request(shell: &Path, input: &RunInput) -> Command {
    let mut command = Command::new(shell);
    let kind = ShellKind::from_path(shell);
    match (kind, input) {
        (ShellKind::Cmd, RunInput::Argv(arguments)) => {
            command.arg("/D").arg("/S").arg("/C");
            command.arg(arguments.join(" "));
        }
        (ShellKind::Cmd, RunInput::Script(script)) => {
            command.arg("/D").arg("/S").arg("/C").arg(script);
        }
        (ShellKind::Fish, RunInput::Argv(arguments)) => {
            command
                .args(["-l", "-i", "-c"])
                .arg("$argv")
                .args(arguments);
        }
        (ShellKind::Bash | ShellKind::Zsh, RunInput::Argv(arguments)) => {
            let bootstrap = shell_bootstrap(input);
            command
                .arg(bootstrap.flags())
                .arg(bootstrap.argv_script())
                .arg("yo-run")
                .args(arguments);
        }
        (ShellKind::Posix, RunInput::Argv(arguments)) => {
            command
                .arg("-lc")
                .arg("\"$@\"")
                .arg("yo-run")
                .args(arguments);
        }
        (ShellKind::Fish, RunInput::Script(script)) => {
            command.args(["-l", "-i", "-c"]).arg(script);
        }
        (ShellKind::Bash | ShellKind::Zsh, RunInput::Script(script)) => {
            let bootstrap = shell_bootstrap(input);
            command.arg(bootstrap.flags()).arg(bootstrap.script(script));
        }
        (ShellKind::Posix, RunInput::Script(script)) => {
            command.arg("-lc").arg(script);
        }
    }
    command
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShellBootstrap {
    Login,
    Pyenv,
    Nvm,
    Interactive,
}

impl ShellBootstrap {
    fn flags(self) -> &'static str {
        if self == Self::Interactive {
            "-lic"
        } else {
            "-lc"
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Self::Pyenv => concat!(
                "export PYENV_ROOT=\"${PYENV_ROOT:-$HOME/.pyenv}\"; ",
                "if [ -d \"$PYENV_ROOT/shims\" ]; then ",
                "export PATH=\"$PYENV_ROOT/shims:$PYENV_ROOT/bin:$PATH\"; ",
                "fi; "
            ),
            Self::Nvm => concat!(
                "export NVM_DIR=\"${NVM_DIR:-$HOME/.nvm}\"; ",
                "if [ -s \"$NVM_DIR/nvm.sh\" ]; then . \"$NVM_DIR/nvm.sh\"; fi; "
            ),
            Self::Login | Self::Interactive => "",
        }
    }

    fn argv_script(self) -> String {
        format!("{}\"$@\"", self.prefix())
    }

    fn script(self, script: &str) -> String {
        format!("{}{script}", self.prefix())
    }
}

fn shell_bootstrap(input: &RunInput) -> ShellBootstrap {
    let Some(program) = first_program(input) else {
        return ShellBootstrap::Interactive;
    };
    match command_name(&program).as_str() {
        "python" | "python3" | "pip" | "pip3" | "pyenv" => ShellBootstrap::Pyenv,
        "node" | "npm" | "npx" | "yarn" | "pnpm" | "nvm" => ShellBootstrap::Nvm,
        program if is_shell_builtin(program) || executable_on_path(program) => {
            ShellBootstrap::Login
        }
        _ => ShellBootstrap::Interactive,
    }
}

fn first_program(input: &RunInput) -> Option<String> {
    match input {
        RunInput::Argv(arguments) => arguments.first().cloned(),
        RunInput::Script(script) => script
            .split_whitespace()
            .find(|word| !is_environment_assignment(word))
            .map(|word| {
                word.trim_matches(|character| matches!(character, '\'' | '"' | '`'))
                    .to_owned()
            }),
    }
}

fn is_shell_builtin(program: &str) -> bool {
    matches!(
        program,
        ":" | "."
            | "alias"
            | "bg"
            | "break"
            | "cd"
            | "command"
            | "continue"
            | "echo"
            | "eval"
            | "exec"
            | "exit"
            | "export"
            | "false"
            | "fg"
            | "jobs"
            | "printf"
            | "pwd"
            | "read"
            | "return"
            | "set"
            | "shift"
            | "source"
            | "test"
            | "trap"
            | "true"
            | "type"
            | "ulimit"
            | "umask"
            | "unalias"
            | "unset"
            | "wait"
    )
}

fn executable_on_path(program: &str) -> bool {
    let path = Path::new(program);
    if path.components().count() > 1 {
        return path.is_file();
    }
    env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join(program))
        .any(|candidate| candidate.is_file())
}

fn read_bounded_tail_reporting<R, F>(
    mut reader: R,
    limit: usize,
    mut on_chunk: F,
) -> io::Result<CapturedStream>
where
    R: Read,
    F: FnMut(&[u8]),
{
    let mut tail = VecDeque::with_capacity(limit.min(8 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    let mut total_bytes = 0_usize;

    loop {
        let bytes_read = reader.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        on_chunk(&buffer[..bytes_read]);
        total_bytes = total_bytes.saturating_add(bytes_read);
        if limit == 0 {
            continue;
        }

        if bytes_read >= limit {
            tail.clear();
            tail.extend(buffer[bytes_read - limit..bytes_read].iter().copied());
            continue;
        }

        let overflow = tail.len().saturating_add(bytes_read).saturating_sub(limit);
        for _ in 0..overflow {
            tail.pop_front();
        }
        tail.extend(buffer[..bytes_read].iter().copied());
    }

    let bytes: Vec<u8> = tail.into_iter().collect();
    let omitted_bytes = total_bytes.saturating_sub(bytes.len());
    Ok(CapturedStream {
        text: String::from_utf8_lossy(&bytes).into_owned(),
        truncated: omitted_bytes > 0,
        omitted_bytes,
    })
}

fn join_reader(
    handle: thread::JoinHandle<io::Result<CapturedStream>>,
    stream_name: &str,
) -> io::Result<CapturedStream> {
    handle
        .join()
        .map_err(|_| io::Error::other(format!("{stream_name} capture thread panicked")))?
}

fn format_run_input(input: &RunInput) -> String {
    match input {
        RunInput::Argv(arguments) => redact_argv(arguments)
            .iter()
            .map(|argument| quote_for_display(argument))
            .collect::<Vec<_>>()
            .join(" "),
        RunInput::Script(script) => redact_secrets(script),
    }
}

fn quote_for_display(argument: &str) -> String {
    if !argument.is_empty()
        && argument
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-._/:@%+=,".contains(character))
    {
        return argument.to_string();
    }
    format!("'{}'", argument.replace('\'', "'\\''"))
}

/// Redact common credential forms from arbitrary terminal text.
///
/// This is intentionally conservative rather than a credential validator. It
/// recognizes common assignments, flags, bearer tokens, provider prefixes, and
/// JWT-shaped values without introducing a regex dependency.
pub fn redact_secrets(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut token_start = 0_usize;
    let mut state = RedactionState::None;

    for (index, character) in input.char_indices() {
        if character.is_whitespace() {
            if token_start < index {
                output.push_str(&redact_token(&input[token_start..index], &mut state));
            }
            output.push(character);
            token_start = index + character.len_utf8();
        }
    }
    if token_start < input.len() {
        output.push_str(&redact_token(&input[token_start..], &mut state));
    }
    redact_sensitive_environment_values(output)
}

fn redact_sensitive_environment_values(mut output: String) -> String {
    let mut values = env::vars()
        .filter_map(|(name, value)| {
            let normalized = name.to_ascii_uppercase();
            (crate::sandbox::is_sensitive_environment_name(&normalized) && value.len() >= 8)
                .then_some(value)
        })
        .collect::<Vec<_>>();
    values.sort_by_key(|value| std::cmp::Reverse(value.len()));
    values.dedup();
    for value in values {
        output = output.replace(&value, "[REDACTED]");
    }
    output
}

fn redact_argv(arguments: &[String]) -> Vec<String> {
    let mut state = RedactionState::None;
    arguments
        .iter()
        .map(|argument| redact_token(argument, &mut state))
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RedactionState {
    None,
    SecretValue,
    AuthorizationValue,
}

fn redact_token(token: &str, state: &mut RedactionState) -> String {
    match *state {
        RedactionState::SecretValue => {
            *state = RedactionState::None;
            return redact_value(token);
        }
        RedactionState::AuthorizationValue => {
            if trim_token_punctuation(token).eq_ignore_ascii_case("bearer") {
                *state = RedactionState::SecretValue;
                return token.to_string();
            }
            *state = RedactionState::None;
            return redact_value(token);
        }
        RedactionState::None => {}
    }

    let inspected = trim_token_punctuation(token);
    if is_sensitive_flag(inspected) {
        *state = RedactionState::SecretValue;
        return token.to_string();
    }
    if inspected.eq_ignore_ascii_case("bearer") {
        *state = RedactionState::SecretValue;
        return token.to_string();
    }

    if let Some((delimiter_index, delimiter)) = assignment_delimiter(token) {
        let key = &token[..delimiter_index];
        if is_sensitive_key(key) {
            let value_start = delimiter_index + delimiter.len_utf8();
            let value = &token[value_start..];
            if value.is_empty() {
                *state = if normalize_key(key) == "authorization" {
                    RedactionState::AuthorizationValue
                } else {
                    RedactionState::SecretValue
                };
                return token.to_string();
            }
            if normalize_key(key) == "authorization"
                && trim_token_punctuation(value).eq_ignore_ascii_case("bearer")
            {
                *state = RedactionState::SecretValue;
                return token.to_string();
            }
            return format!(
                "{}{}{}",
                &token[..delimiter_index],
                delimiter,
                redact_value(value)
            );
        }
    }

    redact_known_token_shapes(token)
}

fn trim_token_punctuation(token: &str) -> &str {
    token.trim_matches(|character: char| {
        matches!(
            character,
            '\'' | '"' | '`' | '[' | ']' | '{' | '}' | '(' | ')' | ',' | ';'
        )
    })
}

fn assignment_delimiter(token: &str) -> Option<(usize, char)> {
    token
        .char_indices()
        .find(|(_, character)| matches!(character, '=' | ':'))
}

fn normalize_key(key: &str) -> String {
    let tail = key.rsplit(['?', '&', ',', '{']).next().unwrap_or(key);
    tail.trim_matches(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .to_ascii_lowercase()
        .replace('-', "_")
}

fn is_sensitive_key(key: &str) -> bool {
    let key = normalize_key(key);
    matches!(
        key.as_str(),
        "api_key"
            | "apikey"
            | "access_key"
            | "access_token"
            | "auth_token"
            | "authorization"
            | "bearer_token"
            | "client_secret"
            | "cookie"
            | "password"
            | "passwd"
            | "private_key"
            | "refresh_token"
            | "secret"
            | "secret_key"
            | "session_token"
            | "token"
    ) || key.ends_with("_api_key")
        || key.ends_with("_access_key")
        || key.ends_with("_access_token")
        || key.ends_with("_auth_token")
        || key.ends_with("_client_secret")
        || key.ends_with("_password")
        || key.ends_with("_private_key")
        || key.ends_with("_refresh_token")
        || key.ends_with("_secret")
        || key.ends_with("_secret_key")
        || key.ends_with("_session_token")
        || key.ends_with("_token")
        || key.ends_with("_database_url")
        || key.ends_with("_connection_string")
}

fn is_sensitive_flag(value: &str) -> bool {
    let value = value.trim_start_matches('-');
    is_sensitive_key(value)
}

fn redact_value(value: &str) -> String {
    let leading_bytes = value
        .char_indices()
        .take_while(|(_, character)| matches!(character, '\'' | '"' | '`' | '[' | '{' | '('))
        .map(|(index, character)| index + character.len_utf8())
        .last()
        .unwrap_or(0);
    let trailing_bytes = value
        .char_indices()
        .rev()
        .take_while(|(_, character)| {
            matches!(character, '\'' | '"' | '`' | ']' | '}' | ')' | ',' | ';')
        })
        .map(|(index, _)| index)
        .last()
        .unwrap_or(value.len());

    if leading_bytes > trailing_bytes {
        return "[REDACTED]".to_string();
    }
    format!(
        "{}[REDACTED]{}",
        &value[..leading_bytes],
        &value[trailing_bytes..]
    )
}

fn redact_known_token_shapes(token: &str) -> String {
    const PREFIXES: &[&str] = &[
        "sk-ant-",
        "sk-proj-",
        "sk-",
        "github_pat_",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "xoxb-",
        "xoxp-",
        "AKIA",
    ];

    let mut output = token.to_string();
    for prefix in PREFIXES {
        let mut search_from = 0_usize;
        while let Some(relative_start) = output[search_from..].find(prefix) {
            let start = search_from + relative_start;
            let secret_end = output[start..]
                .char_indices()
                .take_while(|(_, character)| {
                    character.is_ascii_alphanumeric()
                        || matches!(character, '-' | '_' | '.' | '/' | '+' | '=')
                })
                .map(|(index, character)| start + index + character.len_utf8())
                .last()
                .unwrap_or(start + prefix.len());
            if secret_end.saturating_sub(start) < prefix.len() + 4 {
                search_from = secret_end;
                continue;
            }
            output.replace_range(start..secret_end, "[REDACTED]");
            search_from = start + "[REDACTED]".len();
        }
    }

    if looks_like_jwt(trim_token_punctuation(&output)) {
        redact_value(&output)
    } else {
        output
    }
}

fn looks_like_jwt(value: &str) -> bool {
    value.len() >= 24
        && value.starts_with("eyJ")
        && value.matches('.').count() == 2
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum CommandRisk {
    ReadOnly,
    Sensitive,
    Unknown,
    Mutating,
    Dangerous,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandAssessment {
    pub risk: CommandRisk,
    pub reason: &'static str,
}

impl CommandAssessment {
    pub fn is_obviously_mutating(self) -> bool {
        matches!(self.risk, CommandRisk::Mutating | CommandRisk::Dangerous)
    }

    /// Conservative policy helper for model-initiated execution. Unknown
    /// commands require confirmation; only recognized read-only commands do not.
    pub fn requires_confirmation_for_model(self) -> bool {
        self.risk != CommandRisk::ReadOnly
    }
}

/// Classify a command for confirmation policy without executing or blocking it.
///
/// This intentionally recognizes only common command shapes. `Unknown` means a
/// model should ask, not that an explicit user request should be rejected.
pub fn classify_command(arguments: &[String]) -> CommandAssessment {
    let mut arguments = arguments;
    if arguments.first().is_some_and(|argument| argument == "--") {
        arguments = &arguments[1..];
    }
    if arguments.is_empty() {
        return assessment(CommandRisk::Unknown, "empty command");
    }

    if has_output_redirection(arguments) {
        return assessment(
            CommandRisk::Mutating,
            "shell output redirection can write files",
        );
    }

    let mut index = 0_usize;
    loop {
        let Some(argument) = arguments.get(index) else {
            return assessment(CommandRisk::Unknown, "environment-only command");
        };
        let command = command_name(argument);
        if command == "env" || command == "command" || command == "nohup" || command == "nice" {
            index += 1;
            while arguments
                .get(index)
                .is_some_and(|value| value.starts_with('-') || is_environment_assignment(value))
            {
                index += 1;
            }
            continue;
        }
        if is_environment_assignment(argument) {
            index += 1;
            continue;
        }
        return classify_program(command.as_str(), &arguments[index + 1..]);
    }
}

pub fn classify_run_input(input: &RunInput) -> CommandAssessment {
    match input {
        RunInput::Argv(arguments) => classify_command(arguments),
        RunInput::Script(script) => classify_shell_script(script),
    }
}

fn classify_program(command: &str, arguments: &[String]) -> CommandAssessment {
    if matches!(command, "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh") {
        if let Some(position) = arguments.iter().position(|argument| argument == "-c") {
            if let Some(script) = arguments.get(position + 1) {
                return classify_shell_script(script);
            }
        }
        return assessment(
            CommandRisk::Unknown,
            "interactive shell can execute arbitrary commands",
        );
    }

    if matches!(
        command,
        "rm" | "rmdir"
            | "dd"
            | "mkfs"
            | "fdisk"
            | "parted"
            | "shutdown"
            | "reboot"
            | "halt"
            | "poweroff"
            | "kill"
            | "killall"
            | "pkill"
            | "sudo"
            | "doas"
    ) {
        return assessment(
            CommandRisk::Dangerous,
            "command can delete data or disrupt the system",
        );
    }

    if accesses_sensitive_data(command, arguments) {
        return assessment(
            CommandRisk::Sensitive,
            "command may read credentials or private shell data",
        );
    }

    if matches!(
        command,
        "cp" | "mv"
            | "touch"
            | "mkdir"
            | "install"
            | "ln"
            | "chmod"
            | "chown"
            | "chgrp"
            | "truncate"
            | "tee"
            | "patch"
            | "apply_patch"
            | "wget"
    ) {
        return assessment(
            CommandRisk::Mutating,
            "command commonly changes files or permissions",
        );
    }

    match command {
        "git" => classify_git(arguments),
        "nvm" => classify_nvm(arguments),
        "python" | "python3" | "node" | "ruby" => classify_runtime(command, arguments),
        "find" => classify_find(arguments),
        "sed" | "perl" => {
            if arguments.iter().any(|argument| {
                argument == "-i"
                    || argument.starts_with("-i.")
                    || argument.starts_with("--in-place")
                    || (argument.starts_with('-')
                        && !argument.starts_with("--")
                        && argument[1..].contains('i'))
            }) {
                assessment(CommandRisk::Mutating, "in-place editing changes files")
            } else {
                assessment(
                    CommandRisk::ReadOnly,
                    "stream editing without in-place mode",
                )
            }
        }
        "curl" => classify_curl(arguments),
        "cargo" | "npm" | "pnpm" | "yarn" | "bun" | "pip" | "pip3" | "brew" | "apt" | "apt-get"
        | "dnf" | "yum" => classify_package_command(command, arguments),
        "pwd" | "ls" | "echo" | "printf" | "cat" | "head" | "tail" | "less" | "more" | "rg"
        | "grep" | "egrep" | "fgrep" | "which" | "whereis" | "type" | "stat" | "du" | "df"
        | "ps" | "top" | "htop" | "uname" | "whoami" | "id" | "printenv" | "date" | "history"
        | "man" | "wc" | "sort" | "uniq" | "cut" => {
            assessment(CommandRisk::ReadOnly, "recognized read-only command")
        }
        _ => assessment(
            CommandRisk::Unknown,
            "command is not in the conservative classifier",
        ),
    }
}

fn classify_runtime(command: &str, arguments: &[String]) -> CommandAssessment {
    let version_flag = arguments.len() == 1 && matches!(arguments[0].as_str(), "--version" | "-V")
        || command == "node" && arguments.len() == 1 && arguments[0] == "-v";
    if version_flag {
        assessment(CommandRisk::ReadOnly, "runtime version check is read-only")
    } else {
        assessment(
            CommandRisk::Unknown,
            "runtime can execute code and needs review",
        )
    }
}

fn classify_nvm(arguments: &[String]) -> CommandAssessment {
    let subcommand = arguments
        .iter()
        .find(|argument| !argument.starts_with('-'))
        .map(String::as_str)
        .unwrap_or_default();
    if matches!(subcommand, "list" | "ls" | "current" | "which" | "version") {
        assessment(CommandRisk::ReadOnly, "recognized read-only nvm subcommand")
    } else {
        assessment(CommandRisk::Unknown, "unrecognized nvm subcommand")
    }
}

fn accesses_sensitive_data(command: &str, arguments: &[String]) -> bool {
    if matches!(command, "printenv" | "history") {
        return true;
    }
    if !matches!(
        command,
        "cat"
            | "head"
            | "tail"
            | "less"
            | "more"
            | "rg"
            | "grep"
            | "egrep"
            | "fgrep"
            | "find"
            | "stat"
            | "wc"
            | "sed"
            | "perl"
    ) {
        return false;
    }
    let joined = arguments.join(" ").to_ascii_lowercase();
    [
        "/.ssh/",
        ".ssh/id_",
        "/.aws/",
        "/.gnupg/",
        "/.kube/config",
        "/.docker/config.json",
        "/.config/gcloud/",
        "/etc/shadow",
        "/etc/sudoers",
        ".netrc",
        ".npmrc",
        ".pypirc",
        "credentials.json",
        "service-account",
        "private-key",
        "private_key",
    ]
    .iter()
    .any(|fragment| joined.contains(fragment))
}

fn classify_git(arguments: &[String]) -> CommandAssessment {
    let Some(subcommand) = arguments.iter().find(|argument| !argument.starts_with('-')) else {
        return assessment(CommandRisk::ReadOnly, "git without a mutating subcommand");
    };
    match subcommand.as_str() {
        "status" | "diff" | "log" | "show" | "blame" | "rev-parse" | "ls-files" | "grep" => {
            assessment(CommandRisk::ReadOnly, "recognized read-only git subcommand")
        }
        "remote"
            if !arguments.iter().any(|argument| {
                matches!(
                    argument.as_str(),
                    "add" | "remove" | "rename" | "set-url" | "set-head"
                )
            }) =>
        {
            assessment(CommandRisk::ReadOnly, "git remote inspection is read-only")
        }
        "reset" if arguments.iter().any(|argument| argument == "--hard") => {
            assessment(CommandRisk::Dangerous, "git reset --hard can discard work")
        }
        "clean"
            if arguments
                .iter()
                .any(|argument| argument == "-f" || argument.contains('f')) =>
        {
            assessment(
                CommandRisk::Dangerous,
                "git clean -f deletes untracked files",
            )
        }
        "checkout" if arguments.iter().any(|argument| argument == "--") => assessment(
            CommandRisk::Dangerous,
            "git checkout -- can discard worktree changes",
        ),
        "add" | "commit" | "push" | "pull" | "merge" | "rebase" | "reset" | "clean"
        | "checkout" | "switch" | "restore" | "stash" | "tag" | "branch" | "remote"
        | "cherry-pick" | "revert" | "init" | "clone" => assessment(
            CommandRisk::Mutating,
            "git subcommand changes repository or remote state",
        ),
        _ => assessment(CommandRisk::Unknown, "unrecognized git subcommand"),
    }
}

fn classify_find(arguments: &[String]) -> CommandAssessment {
    if arguments.iter().any(|argument| argument == "-delete") {
        return assessment(CommandRisk::Dangerous, "find -delete removes files");
    }
    if arguments
        .iter()
        .any(|argument| matches!(argument.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir"))
    {
        return assessment(CommandRisk::Unknown, "find can execute another command");
    }
    assessment(CommandRisk::ReadOnly, "find without delete or exec actions")
}

fn classify_curl(arguments: &[String]) -> CommandAssessment {
    let mut mutating = false;
    for (index, argument) in arguments.iter().enumerate() {
        if matches!(
            argument.as_str(),
            "-o" | "--output" | "-O" | "--remote-name" | "-T" | "--upload-file"
        ) || argument.starts_with("--output=")
            || argument.starts_with("--upload-file=")
            || (argument.starts_with("-o") && argument.len() > 2)
            || (argument.starts_with("-T") && argument.len() > 2)
        {
            mutating = true;
        }
        if matches!(
            argument.as_str(),
            "-d" | "--data" | "--data-raw" | "--json" | "-F" | "--form"
        ) {
            return assessment(
                CommandRisk::Mutating,
                "curl request can change remote state",
            );
        }
        if matches!(argument.as_str(), "-X" | "--request")
            && arguments.get(index + 1).is_some_and(|method| {
                !matches!(
                    method.to_ascii_uppercase().as_str(),
                    "GET" | "HEAD" | "OPTIONS"
                )
            })
        {
            return assessment(
                CommandRisk::Mutating,
                "curl uses a state-changing HTTP method",
            );
        }
    }
    if mutating {
        assessment(CommandRisk::Mutating, "curl writes or uploads a file")
    } else {
        assessment(CommandRisk::ReadOnly, "curl request appears read-only")
    }
}

fn classify_package_command(command: &str, arguments: &[String]) -> CommandAssessment {
    let subcommand = arguments
        .iter()
        .find(|argument| !argument.starts_with('-'))
        .map(String::as_str)
        .unwrap_or_default();
    if matches!(
        subcommand,
        "install"
            | "add"
            | "remove"
            | "uninstall"
            | "update"
            | "upgrade"
            | "publish"
            | "link"
            | "unlink"
            | "fix"
            | "prune"
    ) {
        return assessment(
            CommandRisk::Mutating,
            "package command changes dependencies or system state",
        );
    }
    if matches!(
        subcommand,
        "test" | "check" | "build" | "list" | "ls" | "info" | "view"
    ) {
        return assessment(
            CommandRisk::ReadOnly,
            "recognized inspection or verification command",
        );
    }
    assessment(
        CommandRisk::Unknown,
        match command {
            "cargo" => "unrecognized cargo subcommand",
            _ => "unrecognized package-manager subcommand",
        },
    )
}

fn classify_shell_script(script: &str) -> CommandAssessment {
    let lower = script.to_ascii_lowercase();
    if lower.contains("| sh")
        || lower.contains("|sh")
        || lower.contains("| bash")
        || lower.contains("|bash")
        || lower.contains("git reset --hard")
        || lower.contains("git clean -f")
        || lower.contains(":(){:|:&};:")
    {
        return assessment(
            CommandRisk::Dangerous,
            "shell script contains a destructive execution pattern",
        );
    }
    if contains_shell_command(
        &lower,
        &["rm", "rmdir", "dd", "mkfs", "sudo", "shutdown", "reboot"],
    ) {
        return assessment(
            CommandRisk::Dangerous,
            "shell script invokes a dangerous command",
        );
    }
    if lower.contains('>')
        || contains_shell_command(
            &lower,
            &[
                "touch", "mkdir", "mv", "cp", "tee", "chmod", "chown", "install",
            ],
        )
    {
        return assessment(
            CommandRisk::Mutating,
            "shell script appears to change files or permissions",
        );
    }

    let words: Vec<String> = script.split_whitespace().map(ToOwned::to_owned).collect();
    if words.is_empty() {
        assessment(CommandRisk::Unknown, "empty shell script")
    } else if !script.contains([';', '|', '&', '\n', '\r']) {
        classify_command(&words)
    } else {
        assessment(CommandRisk::Unknown, "compound shell script needs review")
    }
}

fn contains_shell_command(script: &str, commands: &[&str]) -> bool {
    script
        .split(|character: char| character.is_whitespace() || ";|&(){}".contains(character))
        .map(|word| word.trim_matches(|character: char| !character.is_ascii_alphanumeric()))
        .any(|word| commands.contains(&word))
}

fn has_output_redirection(arguments: &[String]) -> bool {
    arguments.iter().any(|argument| {
        matches!(
            argument.as_str(),
            ">" | ">>" | "1>" | "1>>" | "2>" | "2>>" | "&>"
        ) || argument.starts_with(">")
    })
}

fn is_environment_assignment(value: &str) -> bool {
    value.split_once('=').is_some_and(|(key, _)| {
        !key.is_empty()
            && key
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
    })
}

fn command_name(argument: &str) -> String {
    Path::new(argument)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(argument)
        .trim_end_matches(".exe")
        .to_ascii_lowercase()
}

const fn assessment(risk: CommandRisk, reason: &'static str) -> CommandAssessment {
    CommandAssessment { risk, reason }
}

#[cfg(test)]
// These tests serialize process-environment mutation with `ENV_LOCK`. Rust
// exposes that mutation as unsafe because concurrent access would be unsound.
#[allow(unsafe_code, clippy::undocumented_unsafe_blocks)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn session_id_prefers_and_normalizes_environment() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous = env::var(SESSION_ID_ENV).ok();
        unsafe { env::set_var(SESSION_ID_ENV, " shell id/one ") };
        assert_eq!(current_session_id(), "shell_id_one");
        restore_env(SESSION_ID_ENV, previous);
    }

    #[test]
    fn fallback_session_id_is_stable_within_process() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous = env::var(SESSION_ID_ENV).ok();
        unsafe { env::remove_var(SESSION_ID_ENV) };
        let first = current_session_id();
        let second = current_session_id();
        assert_eq!(first, second);
        assert!(first.starts_with("yo-"));
        restore_env(SESSION_ID_ENV, previous);
    }

    #[test]
    fn fallback_uses_a_terminal_hint_across_process_boundaries() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous = env::var("TERM_SESSION_ID").ok();
        unsafe { env::set_var("TERM_SESSION_ID", "terminal-window-42") };
        let first = generate_fallback_session_id();
        let second = generate_fallback_session_id();
        assert_eq!(first, second);
        assert!(first.starts_with("yo-auto-"));
        assert_eq!(stable_hash(b"same"), stable_hash(b"same"));
        assert_ne!(stable_hash(b"same"), stable_hash(b"different"));
        restore_env("TERM_SESSION_ID", previous);
    }

    #[test]
    fn init_snippets_create_per_shell_ids() {
        let bash = shell_init_snippet(ShellKind::Bash);
        let zsh = shell_init_snippet(ShellKind::Zsh);
        let fish = shell_init_snippet(ShellKind::Fish);
        assert!(bash.contains("export YO_SESSION_ID"));
        assert!(bash.contains("$$"));
        assert!(bash.contains("PROMPT_COMMAND"));
        assert!(zsh.contains("add-zsh-hook"));
        assert!(fish.contains("set -gx YO_SESSION_ID"));
        assert!(fish.contains("$fish_pid"));
        assert!(fish.contains("fish_postexec"));
    }

    #[test]
    fn shell_installation_is_idempotent() {
        let root = unique_temp_dir("shell-install");
        let path = root.join(".zshrc");
        fs::write(&path, "export PATH=/tmp\n").unwrap();
        let command = r#"eval "$(yo init zsh)""#;

        assert!(install_shell_integration_at(&path, command).unwrap());
        assert!(!install_shell_integration_at(&path, command).unwrap());

        let installed = fs::read_to_string(&path).unwrap();
        assert_eq!(installed.matches("# >>> yo setup >>>").count(), 1);
        assert!(installed.contains(command));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cli_args_accept_optional_separator() {
        let request = RunRequest::from_cli_args(["--", "printf", "%s", "hello world"]).unwrap();
        assert_eq!(
            request.input,
            RunInput::Argv(vec!["printf".into(), "%s".into(), "hello world".into()])
        );
        assert!(RunRequest::from_cli_args(Vec::<String>::new()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn explicit_run_captures_exit_streams_cwd_and_duration() {
        let cwd = unique_temp_dir("capture");
        let request =
            RunRequest::from_cli_args(["--", "sh", "-c", "printf 'out'; printf 'err' >&2; exit 7"])
                .unwrap()
                .with_shell("/bin/sh")
                .with_cwd(&cwd);
        let result = run_explicit(&request).unwrap();
        assert_eq!(result.stdout.text, "out");
        assert_eq!(result.stderr.text, "err");
        assert_eq!(result.exit_code, Some(7));
        assert!(!result.success);
        assert_eq!(result.cwd, fs::canonicalize(&cwd).unwrap());
        assert!(result.duration >= Duration::ZERO);
        fs::remove_dir_all(cwd).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn explicit_run_reports_output_chunks_as_they_arrive() {
        let request = RunRequest::shell_script("printf first; printf second >&2")
            .unwrap()
            .with_shell("/bin/sh");
        let mut observed = String::new();
        let result =
            run_explicit_with_progress(&request, |_, chunk| observed.push_str(chunk)).unwrap();
        assert!(result.success);
        assert!(observed.contains("first"));
        assert!(observed.contains("second"));
    }

    #[cfg(unix)]
    #[test]
    fn explicit_run_closes_child_stdin() {
        let request = RunRequest::shell_script(
            "if read value; then printf unexpected; else printf stdin-closed; fi",
        )
        .unwrap()
        .with_shell("/bin/sh")
        .with_timeout(Duration::from_secs(1));
        let result = run_explicit(&request).unwrap();
        assert!(result.success);
        assert!(!result.timed_out);
        assert_eq!(result.stdout.text, "stdin-closed");
    }

    #[cfg(unix)]
    #[test]
    fn explicit_run_cleans_up_background_processes_holding_capture_open() {
        let request = RunRequest::shell_script("sleep 30 & printf done")
            .unwrap()
            .with_shell("/bin/sh")
            .with_timeout(Duration::from_secs(1));
        let started = Instant::now();
        let result = run_explicit(&request).unwrap();
        assert!(result.success);
        assert!(!result.timed_out);
        assert_eq!(result.stdout.text, "done");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn managed_runtimes_use_fast_shell_bootstraps() {
        assert_eq!(
            shell_bootstrap(&RunInput::Script("python3 --version".into())),
            ShellBootstrap::Pyenv
        );
        assert_eq!(
            shell_bootstrap(&RunInput::Script("nvm list".into())),
            ShellBootstrap::Nvm
        );
        assert_eq!(
            shell_bootstrap(&RunInput::Script("printf ready".into())),
            ShellBootstrap::Login
        );
    }

    #[cfg(unix)]
    #[test]
    fn capture_keeps_only_the_bounded_tail() {
        let request = RunRequest::shell_script("printf 0123456789")
            .unwrap()
            .with_shell("/bin/sh")
            .with_capture_limit(5);
        let result = run_explicit(&request).unwrap();
        assert_eq!(result.stdout.text, "56789");
        assert!(result.stdout.truncated);
        assert_eq!(result.stdout.omitted_bytes, 5);
    }

    #[cfg(unix)]
    #[test]
    fn interactive_user_shell_resolves_initialized_functions() {
        if !Path::new("/bin/zsh").exists() {
            return;
        }
        let _guard = ENV_LOCK.lock().unwrap();
        let previous = env::var("ZDOTDIR").ok();
        let zdotdir = unique_temp_dir("zsh-functions");
        fs::write(
            zdotdir.join(".zshrc"),
            "yo_test_function() { printf 'function:%s' \"$1\"; }\n",
        )
        .unwrap();
        unsafe { env::set_var("ZDOTDIR", &zdotdir) };

        let request = RunRequest::from_cli_args(["yo_test_function", "works"])
            .unwrap()
            .with_shell("/bin/zsh");
        let result = run_explicit(&request).unwrap();
        assert!(result.success, "{}", result.safe_for_display());
        assert_eq!(result.stdout.text, "function:works");

        restore_env("ZDOTDIR", previous);
        fs::remove_dir_all(zdotdir).unwrap();
    }

    #[test]
    fn redaction_covers_assignments_flags_bearer_and_provider_tokens() {
        let input = concat!(
            "OPENAI_API_KEY=sk-proj-abcdefghijk ",
            "--token github_pat_abcdefghijk ",
            "Authorization: Bearer eyJabcdefgh.ijklmnop.qrstuvwx ",
            "{\"password\":\"hunter2\"}\n",
            "normal output"
        );
        let redacted = redact_secrets(input);
        assert!(!redacted.contains("abcdefghijk"));
        assert!(!redacted.contains("hunter2"));
        assert!(!redacted.contains("qrstuvwx"));
        assert!(redacted.contains("normal output"));
        assert!(redacted.matches("[REDACTED]").count() >= 4);
    }

    #[test]
    fn redaction_removes_generic_token_environment_values_even_when_printed_raw() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous = env::var("NPM_TOKEN").ok();
        unsafe { env::set_var("NPM_TOKEN", "npm-secret-value-123") };
        let redacted = redact_secrets("npm-secret-value-123\nNPM_TOKEN=npm-secret-value-123");
        assert!(!redacted.contains("npm-secret-value-123"));
        assert_eq!(redacted.matches("[REDACTED]").count(), 2);
        restore_env("NPM_TOKEN", previous);
    }

    #[test]
    fn safe_display_does_not_expose_raw_secrets() {
        let result = CommandResult {
            session_id: "session".into(),
            input: RunInput::Argv(vec![
                "tool".into(),
                "--api-key".into(),
                "sk-secretvalue".into(),
            ]),
            shell: "/bin/sh".into(),
            cwd: "/tmp".into(),
            stdout: CapturedStream {
                text: "token=github_pat_verysecret".into(),
                truncated: false,
                omitted_bytes: 0,
            },
            stderr: CapturedStream {
                text: "Authorization: Bearer eyJaaaaaaa.bbbbbbbb.cccccccc".into(),
                truncated: false,
                omitted_bytes: 0,
            },
            exit_code: Some(0),
            success: true,
            timed_out: false,
            duration: Duration::from_millis(12),
        };
        let rendered = result.safe_for_display().to_string();
        assert!(!rendered.contains("secretvalue"));
        assert!(!rendered.contains("verysecret"));
        assert!(!rendered.contains("cccccccc"));
        assert!(rendered.contains("[REDACTED]"));
    }

    #[test]
    fn classifier_distinguishes_read_only_mutating_and_dangerous_commands() {
        assert_eq!(classify(&["ls", "-la"]).risk, CommandRisk::ReadOnly);
        assert_eq!(classify(&["touch", "file"]).risk, CommandRisk::Mutating);
        assert_eq!(
            classify(&["cat", "~/.ssh/id_ed25519"]).risk,
            CommandRisk::Sensitive
        );
        assert_eq!(classify(&["printenv"]).risk, CommandRisk::Sensitive);
        assert_eq!(classify(&["nvm", "list"]).risk, CommandRisk::ReadOnly);
        assert_eq!(classify(&["nvm", "use", "20"]).risk, CommandRisk::Unknown);
        assert_eq!(
            classify(&["python3", "--version"]).risk,
            CommandRisk::ReadOnly
        );
        assert_eq!(
            classify(&["python3", "-c", "print(1)"]).risk,
            CommandRisk::Unknown
        );
        assert_eq!(
            classify(&["rm", "-rf", "/tmp/x"]).risk,
            CommandRisk::Dangerous
        );
        assert_eq!(
            classify(&["git", "reset", "--hard"]).risk,
            CommandRisk::Dangerous
        );
        assert_eq!(classify(&["custom-tool"]).risk, CommandRisk::Unknown);
        assert!(classify(&["custom-tool"]).requires_confirmation_for_model());
        assert!(!classify(&["rg", "needle"]).requires_confirmation_for_model());
    }

    #[cfg(unix)]
    #[test]
    fn explicit_run_is_not_blocked_by_advisory_risk() {
        let cwd = unique_temp_dir("explicit");
        let path = cwd.join("created-by-explicit-run");
        let arguments = vec!["touch".to_string(), path.to_string_lossy().into_owned()];
        assert_eq!(classify_command(&arguments).risk, CommandRisk::Mutating);

        let request = RunRequest::from_cli_args(arguments)
            .unwrap()
            .with_shell("/bin/sh");
        let result = run_explicit(&request).unwrap();
        assert!(result.success);
        assert!(path.exists());
        fs::remove_dir_all(cwd).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn timed_commands_stop_and_report_timeout() {
        let request = RunRequest::shell_script("sleep 5")
            .unwrap()
            .with_shell("/bin/sh")
            .with_timeout(Duration::from_millis(75));
        let result = run_explicit(&request).unwrap();
        assert!(result.timed_out);
        assert!(!result.success);
        assert!(result.duration < Duration::from_secs(2));
        assert!(result
            .safe_for_display()
            .to_string()
            .contains("exit: timeout"));
    }

    fn classify(arguments: &[&str]) -> CommandAssessment {
        classify_command(
            &arguments
                .iter()
                .map(|argument| (*argument).to_string())
                .collect::<Vec<_>>(),
        )
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        let path = env::temp_dir().join(format!(
            "yo-terminal-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn restore_env(key: &str, previous: Option<String>) {
        if let Some(value) = previous {
            unsafe { env::set_var(key, value) };
        } else {
            unsafe { env::remove_var(key) };
        }
    }
}
