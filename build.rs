// Embed the git commit this binary was built from, so `rail --version` reports a
// real identity (the Cargo version is a static 0.1.0) and an update check can
// compare it against the latest commit on GitHub.
use std::process::Command;

fn main() {
    let sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=RAIL_GIT_SHA={sha}");
    // Rebuild when HEAD moves so the embedded sha stays current.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads/main");
}
