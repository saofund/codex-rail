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
// updater verifies this marker against the RELEASE's own sha (the commit the
// rolling `latest` tag points at) before it replaces the current binary, so a
// half-uploaded/mismatched asset is rejected while a release that merely lags
// main still installs.
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

/// Resolve a git ref (branch, tag, or sha) to its full commit sha.
fn resolve_ref_sha_full(git_ref: &str) -> Result<String> {
    let v = api_json(&format!("commits/{git_ref}"))
        .with_context(|| format!("resolve git ref {git_ref}"))?;
    let sha = v
        .get("sha")
        .and_then(|s| s.as_str())
        .context("commit response has no sha")?;
    validate_git_sha(sha)?;
    Ok(sha.to_ascii_lowercase())
}

/// GitHub's ahead/behind status of the release relative to the running build.
fn compare_status_to_release(mine: &str, release_sha: &str) -> Result<String> {
    let v = api_json(&format!("compare/{mine}...{release_sha}"))
        .context("compare the running build with the latest release")?;
    v.get("status")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string())
        .context("compare response has no status")
}

/// How the rolling `latest` release relates to the running build.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ReleaseDecision {
    /// The release is strictly ahead of us; the value is its full commit sha.
    Newer(String),
    /// Identical, we are ahead (a dev build past the last CI publish), or the
    /// two lines diverged — nothing to install and nothing to advertise.
    NotNewer,
    /// No embedded sha (unversioned local build) or the comparison failed.
    Unknown,
}

/// Decide, from the running sha, the release sha, and GitHub's compare status,
/// whether the release is a genuine upgrade. Pure so every branch is unit-tested.
///
/// This replaces the old "any sha difference == newer, and the download must
/// match `main` HEAD" logic, which had two real bugs: a rolling release that
/// merely LAGGED main failed every install (marker != main HEAD), and a local
/// build AHEAD of the last publish nagged to "update" (i.e. silently downgrade).
/// Requiring GitHub's `ahead` status fixes both.
fn classify_release(mine: &str, release_sha: &str, compare_status: &str) -> ReleaseDecision {
    let mine = mine.trim().to_ascii_lowercase();
    if mine.is_empty() || mine == "unknown" || validate_git_sha(&mine).is_err() {
        return ReleaseDecision::Unknown;
    }
    let release = release_sha.trim().to_ascii_lowercase();
    if validate_git_sha(&release).is_err() {
        return ReleaseDecision::Unknown;
    }
    if release == mine {
        return ReleaseDecision::NotNewer;
    }
    match compare_status {
        // `status` is head-relative-to-base (base = our build): only "ahead"
        // means the release contains commits we lack. "behind" (we are ahead — a
        // dev build), "identical", and "diverged" must never offer or install.
        "ahead" => ReleaseDecision::Newer(release),
        _ => ReleaseDecision::NotNewer,
    }
}

/// Query GitHub for how the rolling `latest` release compares to this build.
fn release_decision() -> ReleaseDecision {
    let mine = build_sha_full().to_ascii_lowercase();
    if mine == "unknown" {
        return ReleaseDecision::Unknown;
    }
    let Ok(release) = resolve_ref_sha_full("latest") else {
        return ReleaseDecision::Unknown;
    };
    if release == mine {
        return ReleaseDecision::NotNewer;
    }
    let Ok(status) = compare_status_to_release(&mine, &release) else {
        return ReleaseDecision::Unknown;
    };
    classify_release(&mine, &release, &status)
}

fn short_sha(sha: &str) -> Result<String> {
    validate_git_sha(sha)?;
    Ok(sha.chars().take(7).collect())
}

/// If the rolling `latest` release is strictly newer than this build, its short
/// sha; else None. A local build EQUAL TO or AHEAD OF the last publish (the
/// common dev case) returns None — no phantom "update available" note.
pub fn newer_available() -> Option<String> {
    match release_decision() {
        ReleaseDecision::Newer(sha) => short_sha(&sha).ok(),
        ReleaseDecision::NotNewer | ReleaseDecision::Unknown => None,
    }
}

/// Download the latest release binary for this platform and atomically replace
/// the running executable (Linux/macOS let you rename over a running file; the
/// live process keeps the old inode, the next launch picks up the new one).
/// Refuses to downgrade: installs only when the release is genuinely ahead, and
/// validates the download's embedded marker against the RELEASE's sha (not main
/// HEAD), so a rolling release that lags main still installs cleanly instead of
/// failing every time.
pub fn apply() -> Result<String> {
    let exe = current_executable_for_update()?;
    let _guard = acquire_update_lock(&exe)?;
    let expected_sha = match release_decision() {
        ReleaseDecision::Newer(sha) => sha,
        ReleaseDecision::NotNewer => bail!(
            "already up to date — the running build is current with (or newer than) the latest published release"
        ),
        ReleaseDecision::Unknown => bail!(
            "cannot determine the latest release (no network reachable, or this is an unversioned local build)"
        ),
    };
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
    fn classify_release_only_upgrades_when_the_release_is_strictly_ahead() {
        let mine = "0123456789abcdef0123456789abcdef01234567";
        let release = "89abcdef0123456789abcdef0123456789abcdef";

        // The release has commits we lack -> a genuine upgrade.
        assert_eq!(
            classify_release(mine, release, "ahead"),
            ReleaseDecision::Newer(release.to_string())
        );

        // THE BUG THIS FIXES: our build is AHEAD of the last CI publish (the
        // rolling release lags main). The old code saw "different sha" and both
        // nagged and, on click, downgraded. Now every non-"ahead" status is inert.
        assert_eq!(
            classify_release(mine, release, "behind"),
            ReleaseDecision::NotNewer
        );
        assert_eq!(
            classify_release(mine, release, "diverged"),
            ReleaseDecision::NotNewer
        );
        assert_eq!(
            classify_release(mine, release, "identical"),
            ReleaseDecision::NotNewer
        );

        // Same sha is up to date regardless of the (never-"ahead") status.
        assert_eq!(
            classify_release(mine, mine, "ahead"),
            ReleaseDecision::NotNewer
        );

        // Case-insensitive sha match still counts as identical.
        assert_eq!(
            classify_release(mine, &mine.to_ascii_uppercase(), "ahead"),
            ReleaseDecision::NotNewer
        );

        // An unversioned local build, or a garbage release sha, is never acted on.
        assert_eq!(
            classify_release("unknown", release, "ahead"),
            ReleaseDecision::Unknown
        );
        assert_eq!(
            classify_release("", release, "ahead"),
            ReleaseDecision::Unknown
        );
        assert_eq!(
            classify_release(mine, "not-a-real-sha", "ahead"),
            ReleaseDecision::Unknown
        );
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
