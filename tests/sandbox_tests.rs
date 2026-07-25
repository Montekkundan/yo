// `EnvironmentGuard` is used only by the single opt-in OS boundary test. Rust
// exposes process-environment mutation as unsafe because concurrent access
// would be unsound; no other test in this binary runs when that case executes.
#![allow(unsafe_code, clippy::undocumented_unsafe_blocks)]

use std::env;
use std::ffi::OsString;
use std::fs;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::io::Write;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::net::TcpListener;
use std::path::{Path, PathBuf};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::time::Duration;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use yo::sandbox::SandboxBackend;
use yo::sandbox::{
    prepare_with_backend, CommandSpec, FilesystemScopes, NetworkScope, SandboxError, SandboxMode,
    SandboxPolicy,
};

fn existing_program() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(env::var_os("COMSPEC").expect("COMSPEC must name a test shell"))
    } else {
        PathBuf::from("/bin/sh")
    }
}

fn strict_policy(cwd: &Path) -> SandboxPolicy {
    SandboxPolicy::strict(FilesystemScopes::new([cwd], std::iter::empty::<&Path>()).unwrap())
}

struct EnvironmentGuard {
    name: &'static str,
    original: Option<OsString>,
}

impl EnvironmentGuard {
    fn set(name: &'static str, value: &str) -> Self {
        let original = env::var_os(name);
        unsafe { env::set_var(name, value) };
        Self { name, original }
    }
}

impl Drop for EnvironmentGuard {
    fn drop(&mut self) {
        match &self.original {
            Some(value) => unsafe { env::set_var(self.name, value) },
            None => unsafe { env::remove_var(self.name) },
        }
    }
}

#[test]
fn strict_mode_fails_closed_without_a_backend() {
    let cwd = fs::canonicalize(env::current_dir().unwrap()).unwrap();
    let spec = CommandSpec::new(existing_program(), ["-c", "pwd"], &cwd);
    let error = prepare_with_backend(&spec, &strict_policy(&cwd), None).unwrap_err();
    assert!(matches!(error, SandboxError::BackendUnavailable));
}

#[test]
fn best_effort_mode_is_explicit_about_direct_fallback() {
    let cwd = fs::canonicalize(env::current_dir().unwrap()).unwrap();
    let spec = CommandSpec::new(existing_program(), ["-c", "pwd"], &cwd);
    let mut policy = strict_policy(&cwd);
    policy.mode = SandboxMode::BestEffort;
    let plan = prepare_with_backend(&spec, &policy, None).unwrap();
    assert_eq!(plan.backend, None);
    assert_eq!(
        plan.arguments,
        vec![OsString::from("-c"), OsString::from("pwd")]
    );
}

#[test]
fn untrusted_fallback_removes_sensitive_environment_by_default() {
    const NAME: &str = "YO_SANDBOX_TEST_API_KEY";
    let _guard = EnvironmentGuard::set(NAME, "not-a-real-secret");
    let cwd = fs::canonicalize(env::current_dir().unwrap()).unwrap();
    let spec = CommandSpec::new(existing_program(), ["-c", "pwd"], &cwd);
    let mut policy = strict_policy(&cwd);
    policy.mode = SandboxMode::BestEffort;
    let plan = prepare_with_backend(&spec, &policy, None).unwrap();
    assert!(plan.removed_environment.iter().any(|name| name == NAME));

    policy.allow_environment_variable(NAME);
    let allowed = prepare_with_backend(&spec, &policy, None).unwrap();
    assert!(!allowed.removed_environment.iter().any(|name| name == NAME));
}

#[test]
fn generic_tokens_and_database_urls_are_removed_from_child_environments() {
    const TOKEN: &str = "NPM_TOKEN";
    const DATABASE: &str = "DATABASE_URL";
    let _token = EnvironmentGuard::set(TOKEN, "npm-secret-value-123");
    let _database = EnvironmentGuard::set(DATABASE, "postgres://user:pass@localhost/db");
    let cwd = fs::canonicalize(env::current_dir().unwrap()).unwrap();
    let spec = CommandSpec::new(existing_program(), ["-c", "env"], &cwd);
    let mut policy = strict_policy(&cwd);
    policy.mode = SandboxMode::BestEffort;
    let plan = prepare_with_backend(&spec, &policy, None).unwrap();
    assert!(plan.removed_environment.iter().any(|name| name == TOKEN));
    assert!(plan.removed_environment.iter().any(|name| name == DATABASE));
    let output = plan.into_command().output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains("npm-secret-value-123"));
    assert!(!stdout.contains("postgres://user:pass@localhost/db"));
}

#[test]
fn working_directory_must_be_declared() {
    let cwd = fs::canonicalize(env::current_dir().unwrap()).unwrap();
    let unrelated = env::temp_dir().join(format!(
        "yo-unrelated-scope-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&unrelated).unwrap();
    let scopes = FilesystemScopes::new([&unrelated], std::iter::empty::<PathBuf>()).unwrap();
    let spec = CommandSpec::new(existing_program(), ["-c", "pwd"], &cwd);
    let error = prepare_with_backend(&spec, &SandboxPolicy::strict(scopes), None).unwrap_err();
    assert!(matches!(error, SandboxError::CwdOutsideScope(_)));
    fs::remove_dir_all(unrelated).unwrap();
}

#[test]
fn write_scope_implies_read_and_deduplicates_children() {
    let cwd = fs::canonicalize(env::current_dir().unwrap()).unwrap();
    let child = cwd.join("src");
    let scopes = FilesystemScopes::new([&child], [&cwd]).unwrap();
    assert!(scopes.readable_paths().is_empty());
    assert_eq!(scopes.writable_paths(), &[cwd]);
}

#[test]
fn relative_scopes_are_rejected() {
    let error = FilesystemScopes::new(["relative/path"], std::iter::empty::<&str>()).unwrap_err();
    assert!(matches!(error, SandboxError::InvalidScope { .. }));
}

#[test]
fn nonexistent_scopes_are_rejected_before_backend_execution() {
    let missing = env::temp_dir().join(format!("yo-missing-sandbox-scope-{}", std::process::id()));
    let error = FilesystemScopes::new([missing], std::iter::empty::<PathBuf>()).unwrap_err();
    assert!(matches!(error, SandboxError::InvalidScope { .. }));
}

#[cfg(target_os = "linux")]
#[test]
fn bubblewrap_plan_denies_network_and_binds_only_declared_write_paths() {
    let cwd = fs::canonicalize(env::current_dir().unwrap()).unwrap();
    let mut scopes = FilesystemScopes::new([&cwd], std::iter::empty::<&Path>()).unwrap();
    let writable = env::temp_dir();
    scopes.allow_write(&writable).unwrap();
    let policy = SandboxPolicy::strict(scopes);
    let spec = CommandSpec::new(existing_program(), ["-c", "pwd"], &cwd);
    let plan = prepare_with_backend(
        &spec,
        &policy,
        Some(SandboxBackend::Bubblewrap(PathBuf::from("/test/bwrap"))),
    )
    .unwrap();
    let args = plan
        .arguments
        .iter()
        .map(|value| value.to_string_lossy())
        .collect::<Vec<_>>();
    assert!(args.iter().any(|value| value == "--unshare-all"));
    assert!(!args.iter().any(|value| value == "--share-net"));
    assert!(args.windows(3).any(|window| {
        window[0] == "--bind"
            && Path::new(window[1].as_ref()) == writable
            && Path::new(window[2].as_ref()) == writable
    }));
}

#[cfg(target_os = "macos")]
#[test]
fn seatbelt_plan_omits_network_permission_when_denied() {
    let cwd = fs::canonicalize(env::current_dir().unwrap()).unwrap();
    let policy = strict_policy(&cwd);
    let spec = CommandSpec::new(existing_program(), ["-c", "pwd"], &cwd);
    let plan = prepare_with_backend(
        &spec,
        &policy,
        Some(SandboxBackend::Seatbelt(PathBuf::from(
            "/usr/bin/sandbox-exec",
        ))),
    )
    .unwrap();
    let profile = plan.arguments[1].to_string_lossy();
    assert!(profile.contains("(deny default)"));
    assert!(profile.contains("(allow file-read* (literal \"/\"))"));
    assert!(profile.contains("(allow file-read* file-write* (literal \"/dev/null\"))"));
    assert!(profile.contains(&cwd.to_string_lossy().to_string()));
    assert!(!profile.contains("(allow network*)"));
}

#[test]
fn explicit_network_access_is_preserved_in_the_policy() {
    let cwd = fs::canonicalize(env::current_dir().unwrap()).unwrap();
    let mut policy = strict_policy(&cwd);
    policy.network = NetworkScope::Allowed;
    assert_eq!(policy.network, NetworkScope::Allowed);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn os_sandbox_enforces_filesystem_network_and_environment_boundaries() {
    if env::var("YO_SANDBOX_OS_TEST").as_deref() != Ok("1") {
        return;
    }
    let backend = yo::sandbox::detect_backend().expect("OS sandbox backend must be installed");
    let root = env::temp_dir().join(format!(
        "yo-os-sandbox-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let allowed = root.join("allowed");
    let denied = root.join("denied");
    fs::create_dir_all(&allowed).unwrap();
    fs::create_dir_all(&denied).unwrap();
    fs::write(allowed.join("read.txt"), "allowed").unwrap();
    fs::write(denied.join("secret.txt"), "secret").unwrap();

    let scopes = FilesystemScopes::new(std::iter::empty::<&Path>(), [&allowed]).unwrap();
    let policy = SandboxPolicy::strict(scopes);
    let shell = existing_program();

    let allowed_spec = CommandSpec::new(
        &shell,
        ["-c", "cat read.txt && printf written > created.txt"],
        &allowed,
    );
    let allowed_output = prepare_with_backend(&allowed_spec, &policy, Some(backend.clone()))
        .unwrap()
        .into_command()
        .output()
        .unwrap();
    assert!(
        allowed_output.status.success(),
        "allowed sandbox command failed: status={:?}, stdout={}, stderr={}",
        allowed_output.status.code(),
        String::from_utf8_lossy(&allowed_output.stdout),
        String::from_utf8_lossy(&allowed_output.stderr)
    );
    assert_eq!(
        fs::read_to_string(allowed.join("created.txt")).unwrap(),
        "written"
    );

    let denied_read = format!("cat {}", denied.join("secret.txt").display());
    let denied_output = prepare_with_backend(
        &CommandSpec::new(&shell, ["-c", denied_read.as_str()], &allowed),
        &policy,
        Some(backend.clone()),
    )
    .unwrap()
    .into_command()
    .output()
    .unwrap();
    assert!(
        !denied_output.status.success(),
        "undeclared read unexpectedly succeeded"
    );

    let denied_write = format!("printf nope > {}", denied.join("created.txt").display());
    let denied_write_output = prepare_with_backend(
        &CommandSpec::new(&shell, ["-c", denied_write.as_str()], &allowed),
        &policy,
        Some(backend.clone()),
    )
    .unwrap()
    .into_command()
    .output()
    .unwrap();
    assert!(!denied_write_output.status.success());
    assert!(!denied.join("created.txt").exists());

    let _token = EnvironmentGuard::set("NPM_TOKEN", "npm-secret-value-123");
    let environment_output = prepare_with_backend(
        &CommandSpec::new(&shell, ["-c", "test -z \"${NPM_TOKEN+x}\""], &allowed),
        &policy,
        Some(backend.clone()),
    )
    .unwrap()
    .into_command()
    .output()
    .unwrap();
    assert!(environment_output.status.success());

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        for _ in 0..200 {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                        .unwrap();
                    return true;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("listener failed: {error}"),
            }
        }
        false
    });
    let url = format!("http://{address}/");
    let network_output = prepare_with_backend(
        &CommandSpec::new("curl", ["--fail", "--max-time", "2", &url], &allowed),
        &policy,
        Some(backend),
    )
    .unwrap()
    .into_command()
    .output()
    .unwrap();
    assert!(
        !network_output.status.success(),
        "denied network request succeeded"
    );
    assert!(!server.join().unwrap(), "sandbox reached the host network");

    fs::remove_dir_all(root).unwrap();
}
