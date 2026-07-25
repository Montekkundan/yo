//! OS-enforced isolation for commands proposed by the assistant.
//!
//! The sandbox has two trust layers:
//! - a small, read-only runtime view required to start the shell and dynamically
//!   linked programs; and
//! - explicit user filesystem scopes, with writes allowed only for declared
//!   paths.
//!
//! Linux uses Bubblewrap namespaces. macOS uses the built-in Seatbelt policy
//! runner (`sandbox-exec`). Strict mode fails closed on unsupported hosts or
//! when the platform backend is not installed.

use std::collections::BTreeSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxMode {
    Disabled,
    BestEffort,
    Strict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkScope {
    Denied,
    Allowed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SandboxBackend {
    Bubblewrap(PathBuf),
    Seatbelt(PathBuf),
}

impl SandboxBackend {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Bubblewrap(_) => "bubblewrap",
            Self::Seatbelt(_) => "seatbelt",
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FilesystemScopes {
    read: Vec<PathBuf>,
    write: Vec<PathBuf>,
}

impl FilesystemScopes {
    pub fn new<I, J, R, W>(read: I, write: J) -> Result<Self, SandboxError>
    where
        I: IntoIterator<Item = R>,
        J: IntoIterator<Item = W>,
        R: AsRef<Path>,
        W: AsRef<Path>,
    {
        let mut scopes = Self::default();
        for path in write {
            scopes.allow_write(path)?;
        }
        for path in read {
            scopes.allow_read(path)?;
        }
        Ok(scopes)
    }

    pub fn allow_read(&mut self, path: impl AsRef<Path>) -> Result<(), SandboxError> {
        let path = normalize_scope(path.as_ref())?;
        if covered_by(&path, &self.write) || covered_by(&path, &self.read) {
            return Ok(());
        }
        self.read.retain(|existing| !existing.starts_with(&path));
        self.read.push(path);
        self.read.sort();
        Ok(())
    }

    pub fn allow_write(&mut self, path: impl AsRef<Path>) -> Result<(), SandboxError> {
        let path = normalize_scope(path.as_ref())?;
        if covered_by(&path, &self.write) {
            return Ok(());
        }
        self.write.retain(|existing| !existing.starts_with(&path));
        self.read.retain(|existing| !existing.starts_with(&path));
        self.write.push(path);
        self.write.sort();
        Ok(())
    }

    pub fn readable_paths(&self) -> &[PathBuf] {
        &self.read
    }

    pub fn writable_paths(&self) -> &[PathBuf] {
        &self.write
    }

    fn can_read(&self, path: &Path) -> bool {
        covered_by(path, &self.read) || covered_by(path, &self.write)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxPolicy {
    pub mode: SandboxMode,
    pub filesystem: FilesystemScopes,
    pub network: NetworkScope,
    /// A private in-memory `/tmp` on Linux. This never grants host writes.
    pub ephemeral_tmp: bool,
    /// Sensitive parent-process variables are removed unless explicitly named
    /// here. This prevents an isolated command from printing the Gateway key.
    pub environment_allowlist: Vec<OsString>,
}

impl SandboxPolicy {
    pub fn strict(filesystem: FilesystemScopes) -> Self {
        Self {
            mode: SandboxMode::Strict,
            filesystem,
            network: NetworkScope::Denied,
            ephemeral_tmp: true,
            environment_allowlist: Vec::new(),
        }
    }

    pub fn disabled() -> Self {
        Self {
            mode: SandboxMode::Disabled,
            filesystem: FilesystemScopes::default(),
            network: NetworkScope::Allowed,
            ephemeral_tmp: false,
            environment_allowlist: Vec::new(),
        }
    }

    pub fn allow_environment_variable(&mut self, name: impl Into<OsString>) {
        let name = name.into();
        if !self.environment_allowlist.contains(&name) {
            self.environment_allowlist.push(name);
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub arguments: Vec<OsString>,
    pub cwd: PathBuf,
}

impl CommandSpec {
    pub fn new<I, S>(program: impl Into<PathBuf>, arguments: I, cwd: impl Into<PathBuf>) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        Self {
            program: program.into(),
            arguments: arguments.into_iter().map(Into::into).collect(),
            cwd: cwd.into(),
        }
    }

    /// Capture the executable, arguments, and working directory from a command
    /// before stdio is attached or it is spawned.
    pub fn from_command(command: &Command, fallback_cwd: &Path) -> Self {
        Self {
            program: PathBuf::from(command.get_program()),
            arguments: command.get_args().map(OsStr::to_os_string).collect(),
            cwd: command
                .get_current_dir()
                .unwrap_or(fallback_cwd)
                .to_path_buf(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxPlan {
    pub backend: Option<SandboxBackend>,
    pub program: PathBuf,
    pub arguments: Vec<OsString>,
    pub cwd: PathBuf,
    pub removed_environment: Vec<OsString>,
}

impl SandboxPlan {
    pub fn is_sandboxed(&self) -> bool {
        self.backend.is_some()
    }

    pub fn into_command(self) -> Command {
        let mut command = Command::new(self.program);
        command.args(self.arguments).current_dir(self.cwd);
        for name in self.removed_environment {
            command.env_remove(name);
        }
        command
    }
}

#[derive(Debug)]
pub enum SandboxError {
    InvalidScope { path: PathBuf, reason: &'static str },
    ProgramNotFound(PathBuf),
    CwdOutsideScope(PathBuf),
    BackendUnavailable,
    UnsupportedBackend(&'static str),
    Io(io::Error),
}

impl fmt::Display for SandboxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidScope { path, reason } => {
                write!(
                    formatter,
                    "invalid sandbox scope {}: {reason}",
                    path.display()
                )
            }
            Self::ProgramNotFound(path) => {
                write!(
                    formatter,
                    "sandbox command was not found: {}",
                    path.display()
                )
            }
            Self::CwdOutsideScope(path) => write!(
                formatter,
                "sandbox working directory is outside the declared filesystem scopes: {}",
                path.display()
            ),
            Self::BackendUnavailable => write!(
                formatter,
                "strict command sandboxing is unavailable on this host"
            ),
            Self::UnsupportedBackend(name) => {
                write!(
                    formatter,
                    "the {name} sandbox backend is not supported on this host"
                )
            }
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for SandboxError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for SandboxError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Locate the strongest supported sandbox backend without executing it.
pub fn detect_backend() -> Option<SandboxBackend> {
    #[cfg(target_os = "linux")]
    {
        find_program("bwrap").map(SandboxBackend::Bubblewrap)
    }
    #[cfg(target_os = "macos")]
    {
        find_program("sandbox-exec").map(SandboxBackend::Seatbelt)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

/// Build an executable command plan using the host's sandbox backend.
pub fn prepare(spec: &CommandSpec, policy: &SandboxPolicy) -> Result<SandboxPlan, SandboxError> {
    prepare_with_backend(spec, policy, detect_backend())
}

/// Build a deterministic plan with an explicitly selected backend.
///
/// This is public so diagnostics and tests can validate a backend without
/// needing to execute untrusted commands.
pub fn prepare_with_backend(
    spec: &CommandSpec,
    policy: &SandboxPolicy,
    backend: Option<SandboxBackend>,
) -> Result<SandboxPlan, SandboxError> {
    let cwd = normalize_existing(&spec.cwd)?;
    let program = resolve_program(&spec.program)?;

    if policy.mode == SandboxMode::Disabled {
        return Ok(direct_plan(spec, program, cwd, Vec::new()));
    }
    if !policy.filesystem.can_read(&cwd) {
        return Err(SandboxError::CwdOutsideScope(cwd));
    }

    let Some(backend) = backend else {
        return match policy.mode {
            SandboxMode::BestEffort => Ok(direct_plan(
                spec,
                program,
                cwd,
                sensitive_environment(policy),
            )),
            SandboxMode::Strict => Err(SandboxError::BackendUnavailable),
            SandboxMode::Disabled => unreachable!("disabled mode returned above"),
        };
    };

    match backend {
        SandboxBackend::Bubblewrap(ref executable) => {
            build_bubblewrap_plan(spec, policy, &program, &cwd, executable)
        }
        SandboxBackend::Seatbelt(ref executable) => {
            build_seatbelt_plan(spec, policy, &program, &cwd, executable)
        }
    }
}

fn direct_plan(
    spec: &CommandSpec,
    program: PathBuf,
    cwd: PathBuf,
    removed_environment: Vec<OsString>,
) -> SandboxPlan {
    SandboxPlan {
        backend: None,
        program,
        arguments: spec.arguments.clone(),
        cwd,
        removed_environment,
    }
}

fn build_bubblewrap_plan(
    spec: &CommandSpec,
    policy: &SandboxPolicy,
    program: &Path,
    cwd: &Path,
    executable: &Path,
) -> Result<SandboxPlan, SandboxError> {
    if !cfg!(target_os = "linux") {
        return Err(SandboxError::UnsupportedBackend("bubblewrap"));
    }

    let mut arguments: Vec<OsString> = [
        "--die-with-parent",
        "--new-session",
        "--unshare-all",
        "--cap-drop",
        "ALL",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    if policy.network == NetworkScope::Allowed {
        arguments.push("--share-net".into());
    }
    arguments.extend([
        "--proc".into(),
        "/proc".into(),
        "--dev".into(),
        "/dev".into(),
    ]);
    if policy.ephemeral_tmp {
        arguments.extend(["--tmpfs".into(), "/tmp".into()]);
    }

    let runtime = linux_runtime_roots(policy.network);
    let mut read = policy.filesystem.read.clone();
    read.push(program.to_path_buf());
    read.extend(runtime);
    let read = minimize_paths(read, &policy.filesystem.write);
    let parent_directories = mount_parent_directories(read.iter().chain(&policy.filesystem.write));
    for directory in parent_directories {
        arguments.push("--dir".into());
        arguments.push(directory.into_os_string());
    }
    for path in read {
        arguments.push("--ro-bind".into());
        arguments.push(path.as_os_str().to_os_string());
        arguments.push(path.into_os_string());
    }
    for path in &policy.filesystem.write {
        arguments.push("--bind".into());
        arguments.push(path.as_os_str().to_os_string());
        arguments.push(path.as_os_str().to_os_string());
    }
    arguments.push("--chdir".into());
    arguments.push(cwd.as_os_str().to_os_string());
    arguments.push("--".into());
    arguments.push(program.as_os_str().to_os_string());
    arguments.extend(spec.arguments.clone());

    Ok(SandboxPlan {
        backend: Some(SandboxBackend::Bubblewrap(executable.to_path_buf())),
        program: executable.to_path_buf(),
        arguments,
        cwd: cwd.to_path_buf(),
        removed_environment: sensitive_environment(policy),
    })
}

fn build_seatbelt_plan(
    spec: &CommandSpec,
    policy: &SandboxPolicy,
    program: &Path,
    cwd: &Path,
    executable: &Path,
) -> Result<SandboxPlan, SandboxError> {
    if !cfg!(target_os = "macos") {
        return Err(SandboxError::UnsupportedBackend("seatbelt"));
    }

    let mut read = policy.filesystem.read.clone();
    read.push(program.to_path_buf());
    read.extend(macos_runtime_roots());
    let read = minimize_paths(read, &policy.filesystem.write);
    let profile = seatbelt_profile(&read, &policy.filesystem.write, policy.network)?;

    let mut arguments = vec!["-p".into(), profile.into(), "--".into()];
    arguments.push(program.as_os_str().to_os_string());
    arguments.extend(spec.arguments.clone());
    Ok(SandboxPlan {
        backend: Some(SandboxBackend::Seatbelt(executable.to_path_buf())),
        program: executable.to_path_buf(),
        arguments,
        cwd: cwd.to_path_buf(),
        removed_environment: sensitive_environment(policy),
    })
}

fn sensitive_environment(policy: &SandboxPolicy) -> Vec<OsString> {
    let allowed = policy
        .environment_allowlist
        .iter()
        .filter_map(|name| name.to_str())
        .map(|name| name.to_ascii_uppercase())
        .collect::<BTreeSet<_>>();
    let mut removed = vec![OsString::from("ENV"), OsString::from("BASH_ENV")];
    removed.extend(env::vars_os().filter_map(|(name, _)| {
        let normalized = name.to_str()?.to_ascii_uppercase();
        (is_sensitive_environment_name(&normalized) && !allowed.contains(&normalized))
            .then_some(name)
    }));
    removed.sort();
    removed
}

pub(crate) fn is_sensitive_environment_name(name: &str) -> bool {
    matches!(
        name,
        "AI_GATEWAY_API_KEY"
            | "VERCEL_OIDC_TOKEN"
            | "OPENROUTER_API_KEY"
            | "LLMGATEWAY_API_KEY"
            | "LLM_GATEWAY_API_KEY"
            | "GITHUB_TOKEN"
            | "GH_TOKEN"
            | "AUTHORIZATION"
            | "COOKIE"
            | "DATABASE_URL"
            | "MONGODB_URI"
            | "POSTGRES_URL"
            | "REDIS_URL"
    ) || name.ends_with("_API_KEY")
        || name.ends_with("_ACCESS_KEY")
        || name.ends_with("_ACCESS_TOKEN")
        || name.ends_with("_AUTH_TOKEN")
        || name.ends_with("_CLIENT_SECRET")
        || name.ends_with("_PASSWORD")
        || name.ends_with("_PRIVATE_KEY")
        || name.ends_with("_REFRESH_TOKEN")
        || name.ends_with("_SECRET")
        || name.ends_with("_SECRET_KEY")
        || name.ends_with("_SESSION_TOKEN")
        || name.ends_with("_TOKEN")
        || name.ends_with("_DATABASE_URL")
        || name.ends_with("_CONNECTION_STRING")
}

fn seatbelt_profile(
    read: &[PathBuf],
    write: &[PathBuf],
    network: NetworkScope,
) -> Result<String, SandboxError> {
    let mut profile = String::from(
        "(version 1)\n(deny default)\n(allow process*)\n(allow signal (target self))\n(allow sysctl-read)\n(allow mach-lookup)\n(allow file-read* file-write* (literal \"/dev/null\"))\n",
    );
    let mut traversal_paths = BTreeSet::new();
    for path in read.iter().chain(write) {
        let mut current = Some(path.as_path());
        while let Some(candidate) = current {
            traversal_paths.insert(candidate.to_path_buf());
            current = candidate.parent();
        }
    }
    for path in traversal_paths {
        let path = path.to_str().ok_or_else(|| SandboxError::InvalidScope {
            path: path.clone(),
            reason: "Seatbelt requires UTF-8 paths",
        })?;
        profile.push_str("(allow file-read* (literal \"");
        profile.push_str(&escape_seatbelt(path));
        profile.push_str("\"))\n");
    }
    for path in read {
        let path = path.to_str().ok_or_else(|| SandboxError::InvalidScope {
            path: path.clone(),
            reason: "Seatbelt requires UTF-8 paths",
        })?;
        profile.push_str("(allow file-read* (subpath \"");
        profile.push_str(&escape_seatbelt(path));
        profile.push_str("\"))\n");
    }
    for path in write {
        let path = path.to_str().ok_or_else(|| SandboxError::InvalidScope {
            path: path.clone(),
            reason: "Seatbelt requires UTF-8 paths",
        })?;
        profile.push_str("(allow file-read* file-write* (subpath \"");
        profile.push_str(&escape_seatbelt(path));
        profile.push_str("\"))\n");
    }
    if network == NetworkScope::Allowed {
        profile.push_str("(allow network*)\n");
    }
    Ok(profile)
}

fn escape_seatbelt(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len());
    for character in path.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => {
                use fmt::Write;
                let _ = write!(escaped, "\\x{:02x}", character as u32);
            }
            character => escaped.push(character),
        }
    }
    escaped
}

fn linux_runtime_roots(network: NetworkScope) -> Vec<PathBuf> {
    let mut roots = vec![
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/lib64",
        "/nix/store",
        "/run/current-system/sw",
        "/opt/homebrew",
        "/home/linuxbrew/.linuxbrew",
        "/etc/ld.so.cache",
        "/etc/ld.so.conf",
        "/etc/ld.so.conf.d",
        "/etc/passwd",
        "/etc/group",
        "/etc/nsswitch.conf",
        "/etc/hosts",
        "/etc/ssl",
        "/etc/ca-certificates",
    ];
    if network == NetworkScope::Allowed {
        roots.extend(["/etc/resolv.conf", "/etc/gai.conf"]);
    }
    roots
        .into_iter()
        .map(PathBuf::from)
        .filter(|path| path.exists())
        .collect()
}

fn macos_runtime_roots() -> Vec<PathBuf> {
    [
        "/System",
        "/usr",
        "/bin",
        "/sbin",
        "/dev",
        "/private/etc",
        "/private/var/db/dyld",
        "/Library/Apple",
        "/opt/homebrew",
        "/usr/local",
    ]
    .into_iter()
    .map(PathBuf::from)
    .filter(|path| path.exists())
    .collect()
}

fn mount_parent_directories<'a>(paths: impl Iterator<Item = &'a PathBuf>) -> Vec<PathBuf> {
    let mut directories = BTreeSet::new();
    for path in paths {
        let mut parent = path.parent();
        let mut chain = Vec::new();
        while let Some(directory) = parent {
            if directory.parent().is_none() {
                break;
            }
            chain.push(directory.to_path_buf());
            parent = directory.parent();
        }
        directories.extend(chain.into_iter().rev());
    }
    directories.into_iter().collect()
}

fn minimize_paths(mut paths: Vec<PathBuf>, excluded: &[PathBuf]) -> Vec<PathBuf> {
    paths.sort();
    paths.dedup();
    let mut minimized: Vec<PathBuf> = Vec::new();
    for path in paths {
        if covered_by(&path, excluded) || covered_by(&path, &minimized) {
            continue;
        }
        minimized.retain(|existing| !existing.starts_with(&path));
        minimized.push(path);
    }
    minimized.sort();
    minimized
}

fn covered_by(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

fn normalize_existing(path: &Path) -> Result<PathBuf, SandboxError> {
    if !path.is_absolute() {
        return Err(SandboxError::InvalidScope {
            path: path.to_path_buf(),
            reason: "path must be absolute",
        });
    }
    fs::canonicalize(path).map_err(SandboxError::Io)
}

fn normalize_scope(path: &Path) -> Result<PathBuf, SandboxError> {
    if !path.is_absolute() {
        return Err(SandboxError::InvalidScope {
            path: path.to_path_buf(),
            reason: "path must be absolute",
        });
    }
    if !path.exists() {
        return Err(SandboxError::InvalidScope {
            path: path.to_path_buf(),
            reason: "path does not exist",
        });
    }
    fs::canonicalize(path).map_err(SandboxError::Io)
}

fn resolve_program(program: &Path) -> Result<PathBuf, SandboxError> {
    if program.components().count() > 1 || program.is_absolute() {
        return fs::canonicalize(program)
            .map_err(|_| SandboxError::ProgramNotFound(program.to_path_buf()));
    }
    find_program(program.as_os_str())
        .ok_or_else(|| SandboxError::ProgramNotFound(program.to_path_buf()))
}

fn find_program(program: impl AsRef<OsStr>) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join(program.as_ref()))
        .find(|candidate| candidate.is_file())
        .and_then(|candidate| fs::canonicalize(candidate).ok())
}
