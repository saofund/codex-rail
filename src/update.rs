//! Self-update: report the build's version, check GitHub for a newer one, and —
//! once binaries are published as Releases — download the one for this platform
//! and replace the running executable.
//!
//! HTTP goes through `curl` (already present on dev machines and honoring the
//! same HTTPS_PROXY the rest of the tooling uses) so rail stays dependency-free.

use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;
use std::process::Command;

const REPO: &str = "saofund/codex-rail";

/// The git commit this binary was built from (see build.rs).
pub fn build_sha() -> &'static str {
    env!("RAIL_GIT_SHA")
}

/// The release-asset name for the running platform, e.g. `rail-x86_64-linux`.
/// The CI release workflow names its uploads to match.
pub fn asset_name() -> String {
    format!("rail-{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

fn curl(url: &str, timeout: &str) -> Result<Vec<u8>> {
    let out = Command::new("curl")
        .args(["-fsSL", "--max-time", timeout, "-H", "User-Agent: rail-update", url])
        .output()
        .context("run curl (is it installed?)")?;
    if !out.status.success() {
        bail!(
            "curl failed ({}): {}",
            url,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

fn api_json(path: &str) -> Result<serde_json::Value> {
    let body = curl(&format!("https://api.github.com/repos/{REPO}/{path}"), "20")?;
    serde_json::from_slice(&body).context("parse GitHub API response")
}

/// The latest commit sha on `main`, short. The repo has no version tags, so the
/// commit is the version signal. None on any failure (offline, rate-limited) so
/// callers can stay silent.
pub fn latest_main_sha() -> Option<String> {
    let v = api_json("commits/main").ok()?;
    v.get("sha")
        .and_then(|s| s.as_str())
        .map(|s| s.chars().take(7).collect())
}

/// If a newer build than this one is available, the newer short sha; else None.
pub fn newer_available() -> Option<String> {
    let latest = latest_main_sha()?;
    let mine = build_sha();
    if mine != "unknown" && latest != mine {
        Some(latest)
    } else {
        None
    }
}

/// Download the latest release binary for this platform and atomically replace
/// the running executable (Linux/macOS let you rename over a running file; the
/// live process keeps the old inode, the next launch picks up the new one).
/// Errors clearly if no Release/asset exists yet (CI publishes them on push).
pub fn apply() -> Result<String> {
    let rel = api_json("releases/latest")
        .context("fetch latest release (has CI published one yet?)")?;
    let tag = rel
        .get("tag_name")
        .and_then(|t| t.as_str())
        .unwrap_or("latest")
        .to_string();
    let want = asset_name();
    let url = rel
        .get("assets")
        .and_then(|a| a.as_array())
        .into_iter()
        .flatten()
        .find(|a| a.get("name").and_then(|n| n.as_str()) == Some(want.as_str()))
        .and_then(|a| a.get("browser_download_url"))
        .and_then(|u| u.as_str())
        .ok_or_else(|| anyhow!("the latest release has no binary named {want} for this platform"))?;

    let exe = std::env::current_exe().context("locate the running executable")?;
    let tmp = exe.with_extension("update-download");
    let dl = Command::new("curl")
        .args(["-fsSL", "--max-time", "180", "-o"])
        .arg(&tmp)
        .arg(url)
        .output()
        .context("download the new binary")?;
    if !dl.status.success() {
        let _ = std::fs::remove_file(&tmp);
        bail!("download failed: {}", String::from_utf8_lossy(&dl.stderr).trim());
    }
    make_executable(&tmp)?;
    std::fs::rename(&tmp, &exe)
        .with_context(|| format!("replace {}", exe.display()))?;
    Ok(tag)
}

#[cfg(unix)]
fn make_executable(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod {}", p.display()))
}
#[cfg(not(unix))]
fn make_executable(_p: &Path) -> Result<()> {
    Ok(())
}
