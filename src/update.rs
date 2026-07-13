//! Self-update: report the build's version, check GitHub for a newer one, and —
//! once binaries are published as Releases — download the one for this platform
//! and replace the running executable.
//!
//! HTTP goes through `curl` (already present on dev machines and honoring the
//! same HTTPS_PROXY the rest of the tooling uses) so rail stays dependency-free.

use anyhow::{anyhow, bail, Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::{SystemTime, UNIX_EPOCH};

const REPO: &str = "saofund/codex-rail";
const MIN_EXECUTABLE_BYTES: usize = 4096;
const MAX_CURL_BYTES: usize = 64 * 1024 * 1024;
const BUILD_MARKER_PREFIX: &str = "CODEX_RAIL_BUILD_SHA=";
const BUILD_MARKER_SUFFIX: &str = "\n";
static UPDATE_THREAD_LOCK: Mutex<()> = Mutex::new(());
static UPDATE_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

// Keep an exact, cheaply inspectable build identity in every release asset. The
// updater verifies this marker against main before it replaces the current
// binary, so a still-publishing/stale rolling release is rejected.
#[used]
static RAIL_BUILD_SHA_MARKER: &str =
    concat!("CODEX_RAIL_BUILD_SHA=", env!("RAIL_GIT_SHA_FULL"), "\n");

struct UpdateGuard {
    _thread: MutexGuard<'static, ()>,
    _file: File,
}

struct TempCleanup(PathBuf);

impl Drop for TempCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// The git commit this binary was built from (see build.rs).
pub fn build_sha() -> &'static str {
    env!("RAIL_GIT_SHA")
}

pub fn build_sha_full() -> &'static str {
    env!("RAIL_GIT_SHA_FULL")
}

/// The release-asset name for the running platform, e.g. `rail-x86_64-linux`.
/// The CI release workflow names its uploads to match.
pub fn asset_name() -> String {
    format!("rail-{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

fn curl(url: &str, timeout: &str) -> Result<Vec<u8>> {
    let max_bytes = MAX_CURL_BYTES.to_string();
    let out = Command::new("curl")
        .args([
            "--disable",
            "-fsSL",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--max-filesize",
            &max_bytes,
            "--max-time",
            timeout,
            "-H",
            "User-Agent: rail-update",
            url,
        ])
        .output()
        .context("run curl (is it installed?)")?;
    if !out.status.success() {
        bail!(
            "curl failed ({}): {}",
            url,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    if out.stdout.len() > MAX_CURL_BYTES {
        bail!(
            "curl response exceeded the {} MiB safety limit",
            MAX_CURL_BYTES / 1024 / 1024
        );
    }
    Ok(out.stdout)
}

fn api_json(path: &str) -> Result<serde_json::Value> {
    let body = curl(&format!("https://api.github.com/repos/{REPO}/{path}"), "20")?;
    serde_json::from_slice(&body).context("parse GitHub API response")
}

fn validate_git_sha(sha: &str) -> Result<()> {
    if sha.len() < 7 || sha.len() > 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("GitHub returned an invalid commit sha");
    }
    Ok(())
}

fn latest_main_sha_full() -> Result<String> {
    let v = api_json("commits/main").context("fetch latest main commit")?;
    let sha = v
        .get("sha")
        .and_then(|s| s.as_str())
        .context("latest main response has no commit sha")?;
    validate_git_sha(sha)?;
    Ok(sha.to_ascii_lowercase())
}

fn short_sha(sha: &str) -> Result<String> {
    validate_git_sha(sha)?;
    Ok(sha.chars().take(7).collect())
}

/// If a newer build than this one is available, the newer short sha; else None.
pub fn newer_available() -> Option<String> {
    let latest_full = latest_main_sha_full().ok()?;
    let mine = build_sha_full().to_ascii_lowercase();
    if mine != "unknown" && latest_full != mine {
        short_sha(&latest_full).ok()
    } else {
        None
    }
}

/// Download the latest release binary for this platform and atomically replace
/// the running executable (Linux/macOS let you rename over a running file; the
/// live process keeps the old inode, the next launch picks up the new one).
/// Errors clearly if no Release/asset exists yet (CI publishes them on push).
pub fn apply() -> Result<String> {
    let exe = current_executable_for_update()?;
    let _guard = acquire_update_lock(&exe)?;
    let expected_sha = latest_main_sha_full()?;
    let rel =
        api_json("releases/latest").context("fetch latest release (has CI published one yet?)")?;
    let tag = rel
        .get("tag_name")
        .and_then(|t| t.as_str())
        .context("latest release has no tag")?;
    if tag != "latest" {
        bail!("latest GitHub release is not rail's rolling 'latest' release");
    }
    let want = asset_name();
    let url = release_asset_url(&rel, &want)?;
    let bytes = curl(&url, "180").context("download the new binary")?;
    install_download(&exe, &bytes, &expected_sha)?;
    short_sha(&expected_sha)
}

fn current_executable_for_update() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locate the running executable")?;
    let name = exe.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    if name.ends_with(" (deleted)") || name.starts_with(".nfs") {
        bail!("this process already runs a replaced binary; restart rail before updating again");
    }
    let meta = fs::symlink_metadata(&exe)
        .with_context(|| format!("inspect running executable {}", exe.display()))?;
    if !meta.is_file() {
        bail!(
            "running executable is not a regular file: {}",
            exe.display()
        );
    }
    Ok(exe)
}

fn update_lock_path(exe: &Path) -> PathBuf {
    exe.with_extension("update.lock")
}

fn acquire_update_lock(exe: &Path) -> Result<UpdateGuard> {
    let thread = match UPDATE_THREAD_LOCK.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::WouldBlock) => bail!("an update is already in progress"),
        Err(TryLockError::Poisoned(err)) => err.into_inner(),
    };
    let path = update_lock_path(exe);
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("open update lock {}", path.display()))?;
    if !file.metadata()?.is_file() {
        bail!("update lock is not a regular file: {}", path.display());
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restrict update lock {}", path.display()))?;
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break;
        }
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        let raw = err.raw_os_error();
        if raw == Some(libc::EWOULDBLOCK) || raw == Some(libc::EAGAIN) {
            bail!("an update is already in progress");
        }
        return Err(err).with_context(|| format!("lock update file {}", path.display()));
    }
    Ok(UpdateGuard {
        _thread: thread,
        _file: file,
    })
}

fn release_asset_url(release: &serde_json::Value, want: &str) -> Result<String> {
    let url = release
        .get("assets")
        .and_then(|a| a.as_array())
        .into_iter()
        .flatten()
        .find(|a| a.get("name").and_then(|n| n.as_str()) == Some(want))
        .and_then(|a| a.get("browser_download_url"))
        .and_then(|u| u.as_str())
        .ok_or_else(|| {
            anyhow!("the latest release has no binary named {want} for this platform")
        })?;
    let prefix = format!("https://github.com/{REPO}/releases/download/");
    if !url.starts_with(&prefix) {
        bail!("release asset URL is outside the expected GitHub repository");
    }
    Ok(url.to_string())
}

fn build_marker(sha: &str) -> String {
    format!("{BUILD_MARKER_PREFIX}{sha}{BUILD_MARKER_SUFFIX}")
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn validate_download(bytes: &[u8], expected_sha: &str) -> Result<()> {
    validate_git_sha(expected_sha)?;
    if bytes.len() < MIN_EXECUTABLE_BYTES {
        bail!("downloaded release is empty or implausibly small");
    }
    validate_native_executable(bytes)?;
    let marker = build_marker(expected_sha);
    if !contains_bytes(bytes, marker.as_bytes()) {
        bail!(
            "release asset is stale or mismatched: expected build {}",
            expected_sha
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_native_executable(bytes: &[u8]) -> Result<()> {
    if bytes.get(..4) != Some(b"\x7fELF")
        || bytes.get(4) != Some(&2)
        || bytes.get(5) != Some(&1)
        || bytes.get(6) != Some(&1)
    {
        bail!("downloaded release is not a 64-bit little-endian ELF executable");
    }
    let kind = u16::from_le_bytes([bytes[16], bytes[17]]);
    if !matches!(kind, 2 | 3) {
        bail!("downloaded ELF has an invalid executable type");
    }
    let machine = u16::from_le_bytes([bytes[18], bytes[19]]);
    let expected = if cfg!(target_arch = "x86_64") {
        62
    } else if cfg!(target_arch = "aarch64") {
        183
    } else {
        bail!("self-update is unsupported on this Linux architecture");
    };
    if machine != expected {
        bail!("downloaded ELF is for the wrong CPU architecture");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn validate_native_executable(bytes: &[u8]) -> Result<()> {
    if bytes.get(..4) != Some(&[0xcf, 0xfa, 0xed, 0xfe]) {
        bail!("downloaded release is not a 64-bit Mach-O executable");
    }
    let cpu = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    let expected = if cfg!(target_arch = "aarch64") {
        0x0100_000c
    } else if cfg!(target_arch = "x86_64") {
        0x0100_0007
    } else {
        bail!("self-update is unsupported on this macOS architecture");
    };
    if cpu != expected {
        bail!("downloaded Mach-O is for the wrong CPU architecture");
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn validate_native_executable(_bytes: &[u8]) -> Result<()> {
    bail!("self-update is supported only on Linux and macOS")
}

fn create_unique_update_file(exe: &Path) -> Result<(PathBuf, File)> {
    let parent = exe
        .parent()
        .context("running executable has no parent directory")?;
    let base = exe.file_name().and_then(|n| n.to_str()).unwrap_or("rail");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for _ in 0..64 {
        let n = UPDATE_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".{base}.update-tmp-{}-{stamp:x}-{n:x}",
            std::process::id()
        ));
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err).with_context(|| format!("create update file {}", path.display()))
            }
        }
    }
    bail!("could not allocate a unique update file")
}

fn install_download(exe: &Path, bytes: &[u8], expected_sha: &str) -> Result<()> {
    // Validate completely before creating or renaming anything next to the live
    // executable. Every failure before the final rename leaves the old binary
    // byte-for-byte intact.
    validate_download(bytes, expected_sha)?;
    let (tmp, mut file) = create_unique_update_file(exe)?;
    let cleanup = TempCleanup(tmp.clone());
    file.write_all(bytes)
        .with_context(|| format!("write update file {}", tmp.display()))?;
    file.set_permissions(fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod update file {}", tmp.display()))?;
    file.sync_all()
        .with_context(|| format!("sync update file {}", tmp.display()))?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() != bytes.len() as u64 || meta.permissions().mode() & 0o111 == 0
    {
        bail!("staged update is not a complete executable file");
    }
    drop(file);
    let parent = exe
        .parent()
        .context("running executable has no parent directory")?;
    let parent_dir = File::open(parent)
        .with_context(|| format!("open update directory {}", parent.display()))?;
    fs::rename(&tmp, exe).with_context(|| format!("replace {}", exe.display()))?;
    parent_dir
        .sync_all()
        .with_context(|| format!("sync update directory {}", parent.display()))?;
    drop(cleanup); // its path no longer exists after the successful rename
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_dir(name: &str) -> std::path::PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("rail-update-{name}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fake_executable(sha: &str) -> Vec<u8> {
        let mut bytes = vec![0_u8; MIN_EXECUTABLE_BYTES];
        if cfg!(target_os = "linux") {
            bytes[..4].copy_from_slice(b"\x7fELF");
            bytes[4] = 2; // ELFCLASS64
            bytes[5] = 1; // little endian
            bytes[6] = 1; // ELF version
            bytes[16..18].copy_from_slice(&3_u16.to_le_bytes()); // ET_DYN
            let machine = if cfg!(target_arch = "x86_64") {
                62_u16
            } else if cfg!(target_arch = "aarch64") {
                183_u16
            } else {
                0_u16
            };
            bytes[18..20].copy_from_slice(&machine.to_le_bytes());
        } else if cfg!(target_os = "macos") {
            bytes[..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]); // MH_MAGIC_64
            let cpu = if cfg!(target_arch = "aarch64") {
                0x0100_000c_u32
            } else {
                0x0100_0007_u32
            };
            bytes[4..8].copy_from_slice(&cpu.to_le_bytes());
        }
        let marker = build_marker(sha);
        bytes[128..128 + marker.len()].copy_from_slice(marker.as_bytes());
        bytes
    }

    #[test]
    fn download_validation_rejects_empty_wrong_format_and_stale_sha() {
        assert!(validate_download(&[], "1234567").is_err());
        assert!(validate_download(&vec![b'x'; MIN_EXECUTABLE_BYTES], "1234567").is_err());
        assert!(validate_download(&fake_executable("7654321"), "1234567").is_err());
        assert!(validate_download(&fake_executable("12345670"), "1234567").is_err());
        assert!(validate_download(&fake_executable("1234567"), "1234567").is_ok());
    }

    #[test]
    fn failed_install_preserves_the_original_binary() {
        let dir = test_dir("preserve");
        let exe = dir.join("rail");
        fs::write(&exe, b"ORIGINAL").unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(install_download(&exe, b"", "1234567").is_err());
        assert_eq!(fs::read(&exe).unwrap(), b"ORIGINAL");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn successful_install_is_atomic_private_and_executable() {
        let dir = test_dir("install");
        let exe = dir.join("rail");
        fs::write(&exe, b"ORIGINAL").unwrap();
        let bytes = fake_executable("1234567");

        install_download(&exe, &bytes, "1234567").unwrap();
        assert_eq!(fs::read(&exe).unwrap(), bytes);
        assert_eq!(
            fs::metadata(&exe).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::read_dir(&dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().contains("update-tmp"))
                .count(),
            0
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn update_lock_rejects_a_second_owner_and_releases_cleanly() {
        const CHILD_EXE: &str = "CODEX_RAIL_TEST_UPDATE_LOCK_EXE";
        const CHILD_EXPECT: &str = "CODEX_RAIL_TEST_UPDATE_LOCK_EXPECT";
        if let Some(exe) = std::env::var_os(CHILD_EXE) {
            let result = acquire_update_lock(Path::new(&exe));
            match std::env::var(CHILD_EXPECT).as_deref() {
                Ok("busy") => assert!(result.is_err()),
                Ok("free") => assert!(result.is_ok()),
                other => panic!("unexpected child lock mode: {other:?}"),
            }
            return;
        }

        let dir = test_dir("lock");
        let exe = dir.join("rail");
        fs::write(&exe, b"ORIGINAL").unwrap();
        let lock = update_lock_path(&exe);
        fs::write(&lock, b"").unwrap();
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o666)).unwrap();

        let first = acquire_update_lock(&exe).unwrap();
        assert_eq!(
            fs::metadata(&lock).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(acquire_update_lock(&exe).is_err());

        let harness = std::env::current_exe().unwrap();
        let child = |expect: &str| {
            std::process::Command::new(&harness)
                .args([
                    "--exact",
                    "update::tests::update_lock_rejects_a_second_owner_and_releases_cleanly",
                ])
                .env(CHILD_EXE, &exe)
                .env(CHILD_EXPECT, expect)
                .status()
                .unwrap()
        };
        assert!(child("busy").success());

        drop(first);
        assert!(acquire_update_lock(&exe).is_ok());
        assert!(child("free").success());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn release_asset_must_be_the_expected_https_repo_asset() {
        let rel = serde_json::json!({
            "assets": [{
                "name": asset_name(),
                "browser_download_url": format!(
                    "https://github.com/{REPO}/releases/download/latest/{}",
                    asset_name()
                )
            }]
        });
        assert!(release_asset_url(&rel, &asset_name()).is_ok());

        let evil = serde_json::json!({
            "assets": [{
                "name": asset_name(),
                "browser_download_url": "https://example.invalid/rail"
            }]
        });
        assert!(release_asset_url(&evil, &asset_name()).is_err());
    }
}
