//! Verified in-place updates from Yo's GitHub releases.

use anyhow::{Context, Result};
use flate2::read::GzDecoder;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::Cursor;
use std::path::{Path, PathBuf};

const RELEASES_URL: &str = "https://api.github.com/repos/Montekkundan/yo/releases/latest";

#[derive(Clone, Debug, Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<ReleaseAsset>,
}

#[derive(Clone, Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateCheck {
    pub current: String,
    pub latest: String,
    pub available: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateResult {
    pub version: String,
    pub updated: bool,
    pub signature_verified: bool,
}

pub async fn check() -> Result<UpdateCheck> {
    let release = latest_release().await?;
    let current = env!("CARGO_PKG_VERSION").to_owned();
    let available = version_is_newer(&release.tag_name, &current)?;
    Ok(UpdateCheck {
        current,
        latest: release.tag_name,
        available,
    })
}

pub async fn install_latest() -> Result<UpdateResult> {
    let release = latest_release().await?;
    if !version_is_newer(&release.tag_name, env!("CARGO_PKG_VERSION"))? {
        return Ok(UpdateResult {
            version: release.tag_name,
            updated: false,
            signature_verified: false,
        });
    }

    let archive_name = archive_name(&release.tag_name)?;
    let archive_asset = asset(&release, &archive_name)?;
    let checksum_asset = asset(&release, "SHA256SUMS")?;
    let client = github_client()?;
    let archive = download(&client, archive_asset).await?;
    let checksums = download(&client, checksum_asset).await?;
    verify_checksum(&archive_name, &archive, &checksums)?;

    let temporary = temporary_update_dir(&release.tag_name)?;
    let archive_path = temporary.join(&archive_name);
    fs::write(&archive_path, &archive)?;
    let binary = extract_binary(&archive_name, &archive, &temporary)?;
    verify_release_identity(&client, &release, &archive_name, &archive_path, &temporary).await?;

    self_replace::self_replace(&binary).with_context(|| {
        format!(
            "failed to replace {}; if Yo was installed by a package manager, use that package manager to upgrade",
            std::env::current_exe()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| "the current executable".into())
        )
    })?;
    let _ = fs::remove_dir_all(&temporary);
    Ok(UpdateResult {
        version: release.tag_name,
        updated: true,
        signature_verified: true,
    })
}

async fn latest_release() -> Result<Release> {
    let response = github_client()?
        .get(RELEASES_URL)
        .send()
        .await
        .context("failed to check GitHub releases")?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("no published Yo release exists yet");
    }
    response
        .error_for_status()
        .context("GitHub release check failed")?
        .json()
        .await
        .context("GitHub returned an invalid release response")
}

fn github_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("yo/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(120))
        .build()?)
}

async fn download(client: &reqwest::Client, asset: &ReleaseAsset) -> Result<Vec<u8>> {
    Ok(client
        .get(&asset.browser_download_url)
        .send()
        .await
        .with_context(|| format!("failed to download {}", asset.name))?
        .error_for_status()
        .with_context(|| format!("download failed for {}", asset.name))?
        .bytes()
        .await?
        .to_vec())
}

fn asset<'a>(release: &'a Release, name: &str) -> Result<&'a ReleaseAsset> {
    release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .with_context(|| format!("release {} has no `{name}` asset", release.tag_name))
}

fn version_is_newer(latest: &str, current: &str) -> Result<bool> {
    let latest = Version::parse(latest.trim_start_matches('v'))
        .with_context(|| format!("invalid release version `{latest}`"))?;
    let current = Version::parse(current).context("invalid built-in Yo version")?;
    Ok(latest > current)
}

fn archive_name(version: &str) -> Result<String> {
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        (os, arch) => anyhow::bail!("automatic updates are not published for {os}/{arch}"),
    };
    let extension = if cfg!(windows) { "zip" } else { "tar.gz" };
    Ok(format!("yo-{version}-{target}.{extension}"))
}

fn verify_checksum(name: &str, archive: &[u8], checksum_file: &[u8]) -> Result<()> {
    let source = std::str::from_utf8(checksum_file).context("SHA256SUMS is not UTF-8")?;
    let expected = source.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let file = parts.next()?.trim_start_matches('*');
        (file == name).then_some(hash)
    });
    let expected = expected.with_context(|| format!("SHA256SUMS has no entry for {name}"))?;
    let actual = format!("{:x}", Sha256::digest(archive));
    if !actual.eq_ignore_ascii_case(expected) {
        anyhow::bail!("checksum verification failed for {name}");
    }
    Ok(())
}

fn temporary_update_dir(version: &str) -> Result<PathBuf> {
    let path = crate::config::get_app_dir().join(format!(
        ".update-{}-{}-{}",
        version,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn extract_binary(name: &str, archive: &[u8], directory: &Path) -> Result<PathBuf> {
    let output = directory.join(if cfg!(windows) { "yo.exe" } else { "yo" });
    if name.ends_with(".zip") {
        let mut zip = zip::ZipArchive::new(Cursor::new(archive))?;
        let mut source = zip
            .by_name("yo.exe")
            .context("release archive does not contain yo.exe")?;
        let mut destination = File::create(&output)?;
        std::io::copy(&mut source, &mut destination)?;
    } else {
        let decoder = GzDecoder::new(Cursor::new(archive));
        let mut tar = tar::Archive::new(decoder);
        let mut found = false;
        for entry in tar.entries()? {
            let mut entry = entry?;
            if entry.header().entry_type().is_file()
                && entry.path()?.file_name().is_some_and(|file| file == "yo")
            {
                let mut destination = File::create(&output)?;
                std::io::copy(&mut entry, &mut destination)?;
                found = true;
                break;
            }
        }
        if !found {
            anyhow::bail!("release archive does not contain yo");
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&output, fs::Permissions::from_mode(0o755))?;
    }
    Ok(output)
}

async fn verify_release_identity(
    client: &reqwest::Client,
    release: &Release,
    archive_name: &str,
    archive_path: &Path,
    directory: &Path,
) -> Result<bool> {
    if std::process::Command::new("cosign")
        .arg("version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        let bundle_name = format!("{archive_name}.sigstore.json");
        let bundle_asset = asset(release, &bundle_name)?;
        let bundle_path = directory.join(&bundle_name);
        fs::write(&bundle_path, download(client, bundle_asset).await?)?;
        let identity = format!(
            "https://github.com/Montekkundan/yo/.github/workflows/release.yml@refs/tags/{}",
            release.tag_name
        );
        let status = std::process::Command::new("cosign")
            .args(["verify-blob", "--bundle"])
            .arg(&bundle_path)
            .args(["--certificate-identity", &identity])
            .args([
                "--certificate-oidc-issuer",
                "https://token.actions.githubusercontent.com",
            ])
            .arg(archive_path)
            .status()
            .context("failed to run cosign")?;
        if !status.success() {
            anyhow::bail!("Sigstore verification failed for {archive_name}");
        }
        return Ok(true);
    }

    if std::process::Command::new("gh")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        let identity = format!(
            "https://github.com/Montekkundan/yo/.github/workflows/release.yml@refs/tags/{}",
            release.tag_name
        );
        let source_ref = format!("refs/tags/{}", release.tag_name);
        let status = std::process::Command::new("gh")
            .args(["attestation", "verify"])
            .arg(archive_path)
            .args(["-R", "Montekkundan/yo"])
            .args(["--cert-identity", &identity])
            .args(["--source-ref", &source_ref])
            .args([
                "--signer-workflow",
                "Montekkundan/yo/.github/workflows/release.yml",
            ])
            .arg("--deny-self-hosted-runners")
            .status()
            .context("failed to run GitHub attestation verification")?;
        if !status.success() {
            anyhow::bail!("GitHub attestation verification failed for {archive_name}");
        }
        return Ok(true);
    }

    anyhow::bail!(
        "refusing to update without release identity verification; install `cosign` or GitHub CLI (`gh`)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_verification_selects_the_exact_asset() {
        let bytes = b"release";
        let hash = format!("{:x}", Sha256::digest(bytes));
        let sums = format!("{hash}  yo-2.1.0-target.tar.gz\n");
        verify_checksum("yo-2.1.0-target.tar.gz", bytes, sums.as_bytes()).unwrap();
    }

    #[test]
    fn semantic_versions_are_compared_without_a_v_prefix() {
        assert!(version_is_newer("v2.1.0", "2.0.0").unwrap());
        assert!(!version_is_newer("2.0.0", "2.0.0").unwrap());
    }
}
