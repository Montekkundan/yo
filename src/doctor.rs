//! Local installation and runtime health checks.

use crate::config::{self, SandboxMode as ConfigSandboxMode};
use crate::db;
use crate::gateway::GatewayClient;
use crate::sandbox::{
    self, CommandSpec, FilesystemScopes, NetworkScope, SandboxMode, SandboxPolicy,
};
use crate::{diagnostics, personalize, update};
use anyhow::Result;
use serde::Serialize;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Instant;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
    Skip,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct DoctorCheck {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct DoctorReport {
    pub healthy: bool,
    pub checks: Vec<DoctorCheck>,
}

pub async fn inspect(offline: bool) -> DoctorReport {
    let mut checks = Vec::new();
    let settings = match config::load_or_create_config() {
        Ok(settings) => {
            checks.push(pass(
                "config",
                config::get_config_path().display().to_string(),
            ));
            settings
        }
        Err(error) => {
            checks.push(fail("config", error.to_string()));
            return DoctorReport {
                healthy: false,
                checks,
            };
        }
    };
    checks.push(if settings.model.trim().is_empty() {
        fail("model", "no model selected; run `yo setup`")
    } else {
        pass("model", settings.model.clone())
    });
    checks.push(
        if config::gateway_credential_for(settings.gateway_provider).is_ok() {
            pass(
                "credential",
                format!(
                    "{} via {}",
                    settings.gateway_provider.display_name(),
                    config::gateway_credential_source_for(settings.gateway_provider)
                ),
            )
        } else {
            fail(
                "credential",
                format!(
                    "{} is not configured",
                    settings.gateway_provider.display_name()
                ),
            )
        },
    );

    checks.push(match db::integrity_check_existing() {
        Ok(result) if result == "ok" => pass("database", "integrity check passed"),
        Ok(result) => fail("database", result),
        Err(error) => fail("database", error.to_string()),
    });
    let personalize_path = personalize::path();
    checks.push(if personalize_path.is_file() {
        pass("personalize", personalize_path.display().to_string())
    } else {
        fail("personalize", "personalize.md is missing; run `yo setup`")
    });
    checks.push(sandbox_check(&settings));

    if offline {
        checks.push(skip("gateway network", "skipped by --offline"));
        checks.push(skip("updates", "skipped by --offline"));
    } else {
        if let Ok(key) = config::gateway_credential_for(settings.gateway_provider) {
            let client = GatewayClient::for_provider(settings.gateway_provider, key);
            let started = Instant::now();
            checks.push(match client.list_models().await {
                Ok(models) => pass(
                    "gateway network",
                    format!(
                        "{} models in {} ms",
                        models.len(),
                        started.elapsed().as_millis()
                    ),
                ),
                Err(error) => fail("gateway network", error.to_string()),
            });
        } else {
            checks.push(skip("gateway network", "credential is not configured"));
        }
        checks.push(match update::check().await {
            Ok(status) if status.available => warn(
                "updates",
                format!("{} available; run `yo update`", status.latest),
            ),
            Ok(status) => pass("updates", format!("{} is current", status.current)),
            Err(error) if error.to_string().contains("no published Yo release") => {
                warn("updates", error.to_string())
            }
            Err(error) => warn("updates", error.to_string()),
        });
    }

    checks.push(if command_exists("cosign") || command_exists("gh") {
        pass(
            "release signatures",
            "release identity verification is available",
        )
    } else {
        fail(
            "release signatures",
            "install cosign or GitHub CLI before using `yo update`",
        )
    });
    let events = diagnostics::read_events().unwrap_or_default();
    checks.push(if settings.diagnostics_enabled {
        pass(
            "diagnostics",
            format!("enabled locally; {} events", events.len()),
        )
    } else {
        skip(
            "diagnostics",
            "off by default; enable with `yo diagnostics on`",
        )
    });

    let healthy = checks.iter().all(|check| check.status != CheckStatus::Fail);
    DoctorReport { healthy, checks }
}

pub fn print(report: &DoctorReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
        return Ok(());
    }
    for check in &report.checks {
        let marker = match check.status {
            CheckStatus::Pass => "ok",
            CheckStatus::Warn => "warn",
            CheckStatus::Fail => "fail",
            CheckStatus::Skip => "skip",
        };
        println!("{marker:>4}  {:<20} {}", check.name, check.detail);
    }
    if report.healthy {
        println!("\nYo is healthy.");
    } else {
        println!("\nYo needs attention.");
    }
    Ok(())
}

fn sandbox_check(settings: &config::Config) -> DoctorCheck {
    if settings.sandbox_mode == ConfigSandboxMode::Off {
        return warn("command sandbox", "disabled by configuration");
    }
    let Some(backend) = sandbox::detect_backend() else {
        return if settings.sandbox_mode == ConfigSandboxMode::Required {
            fail(
                "command sandbox",
                "required, but no OS backend is available",
            )
        } else {
            warn(
                "command sandbox",
                "no OS backend; model commands fail closed and explicit commands run directly",
            )
        };
    };
    let cwd = match std::env::current_dir().and_then(std::fs::canonicalize) {
        Ok(path) => path,
        Err(error) => return fail("command sandbox", error.to_string()),
    };
    let program = if cfg!(windows) {
        PathBuf::from("cmd.exe")
    } else {
        PathBuf::from("/usr/bin/true")
    };
    let scopes = match FilesystemScopes::new([&cwd], [&cwd]) {
        Ok(scopes) => scopes,
        Err(error) => return fail("command sandbox", error.to_string()),
    };
    let mut policy = SandboxPolicy::strict(scopes);
    policy.network = if settings.sandbox_network {
        NetworkScope::Allowed
    } else {
        NetworkScope::Denied
    };
    policy.mode = SandboxMode::Strict;
    let arguments = if cfg!(windows) {
        vec!["/D", "/C", "exit", "0"]
    } else {
        Vec::new()
    };
    let spec = CommandSpec::new(&program, arguments, &cwd);
    match sandbox::prepare(&spec, &policy) {
        Ok(plan) => {
            let status = plan
                .into_command()
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            match status {
                Ok(status) if status.success() => pass(
                    "command sandbox",
                    format!(
                        "{} operational; network {}",
                        backend.name(),
                        if settings.sandbox_network {
                            "allowed"
                        } else {
                            "denied"
                        }
                    ),
                ),
                Ok(status) => fail(
                    "command sandbox",
                    format!("{} smoke test exited with {status}", backend.name()),
                ),
                Err(error) => fail("command sandbox", error.to_string()),
            }
        }
        Err(error) => fail("command sandbox", error.to_string()),
    }
}

fn command_exists(name: &str) -> bool {
    Command::new(name)
        .arg("version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn pass(name: impl Into<String>, detail: impl Into<String>) -> DoctorCheck {
    check(name, CheckStatus::Pass, detail)
}

fn warn(name: impl Into<String>, detail: impl Into<String>) -> DoctorCheck {
    check(name, CheckStatus::Warn, detail)
}

fn fail(name: impl Into<String>, detail: impl Into<String>) -> DoctorCheck {
    check(name, CheckStatus::Fail, detail)
}

fn skip(name: impl Into<String>, detail: impl Into<String>) -> DoctorCheck {
    check(name, CheckStatus::Skip, detail)
}

fn check(name: impl Into<String>, status: CheckStatus, detail: impl Into<String>) -> DoctorCheck {
    DoctorCheck {
        name: name.into(),
        status,
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_is_unhealthy_only_for_failed_checks() {
        let checks = [pass("a", "ok"), warn("b", "warning"), skip("c", "skip")];
        assert!(checks.iter().all(|check| check.status != CheckStatus::Fail));
        assert_eq!(fail("d", "bad").status, CheckStatus::Fail);
    }
}
