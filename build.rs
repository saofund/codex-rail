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
    // Rebuild when HEAD moves so the embedded sha stays current.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads/main");
}
