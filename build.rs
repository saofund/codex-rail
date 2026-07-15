// Embed both a short display identity and the full git commit. The updater uses
// the full identity to reject a stale rolling-release asset without relying on
// a seven-character prefix.
use std::process::Command;

fn main() {
    let full_sha = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let short_sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=RAIL_GIT_SHA={short_sha}");
    println!("cargo:rustc-env=RAIL_GIT_SHA_FULL={full_sha}");
    // Rebuild when the checked-out commit moves so the embedded sha stays
    // current — on WHATEVER branch is checked out. The old code hard-coded
    // refs/heads/main, so a commit on any other branch (e.g. a feature branch)
    // left `rail --version` reporting a stale sha. Watch HEAD, packed-refs, and
    // the specific ref HEAD points at.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/packed-refs");
    if let Ok(head) = std::fs::read_to_string(".git/HEAD") {
        if let Some(reference) = head.strip_prefix("ref:").map(str::trim) {
            println!("cargo:rerun-if-changed=.git/{reference}");
        }
    }
}
