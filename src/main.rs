#![cfg(unix)]

mod attach;
mod autopilot;
mod distill;
mod guardian;
mod process_tree;
mod progress;
mod protocol;
mod state;
mod ui;
mod update;
mod worker;

use anyhow::{bail, Result};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args.next();

    // Rail's crash-safety contract currently depends on Linux subreapers and
    // /proc generation censuses.  Other Unix targets can compile the shared
    // code, but must not start a manager/worker that cannot uphold verified
    // descendant cleanup after SIGKILL or OOM.  Keep help/version available so
    // an accidentally copied binary explains itself instead of failing opaquely.
    #[cfg(not(target_os = "linux"))]
    if !matches!(
        command.as_deref(),
        Some("--help") | Some("-h") | Some("--version") | Some("-V")
    ) {
        bail!(
            "Codex Rail is supported only on Linux: verified process-generation cleanup requires Linux subreapers and /proc"
        );
    }

    match command.as_deref() {
        Some("--worker") => {
            let Some(id) = args.next() else {
                bail!("missing worker session id");
            };
            worker::run_worker(&id)
        }
        Some("--guardian") => {
            let id = args
                .next()
                .ok_or_else(|| anyhow::anyhow!("missing session id"))?;
            if args.next().is_some() {
                bail!("unexpected extra guardian arguments");
            }
            guardian::run_guardian(&id)
        }
        Some("--help") | Some("-h") => {
            print_help();
            Ok(())
        }
        Some("--version") | Some("-V") => {
            println!(
                "rail {} ({})",
                env!("CARGO_PKG_VERSION"),
                env!("RAIL_GIT_SHA")
            );
            Ok(())
        }
        Some("update") => {
            println!(
                "rail {} ({})",
                env!("CARGO_PKG_VERSION"),
                update::build_sha()
            );
            match update::newer_available() {
                Some(latest) => {
                    println!("newer version available: {latest} — downloading…");
                    let tag = update::apply()?;
                    println!("updated to {tag}. Restart rail to run the new version.");
                }
                None => println!("already up to date (or offline / GitHub unreachable)."),
            }
            Ok(())
        }
        // Diagnostic/headless: aggregate the archive corpus without launching a
        // codex session, and print the plan (used by tests and for timing).
        Some("--distill-prepare") => {
            let mut prep = distill::prepare()?;
            // Also drop the exact prompt the UI would launch codex with, so a
            // headless/real-codex test can drive the same thing.
            let prompt_path = prep.workdir.join(".last-distill-prompt.txt");
            state::write_private_file(&prompt_path, distill::distill_prompt(&prep))?;
            println!(
                "distill: {} sessions ({} codex + {} claude) · {} user turns · {} chunk(s) · scanned {} files, {} rich sessions available -> {}/{}",
                prep.sessions,
                prep.codex_sessions,
                prep.claude_sessions,
                prep.messages,
                prep.chunks.len(),
                prep.scanned,
                prep.available,
                prep.workdir.display(),
                prep.corpus_rel
            );
            println!(
                "next output: {}/{}",
                prep.workdir.display(),
                prep.output_file
            );
            println!("prompt: {}", prompt_path.display());
            for c in &prep.chunks {
                println!("  {} id={}", c.file, c.marker);
            }
            // Headless prepare intentionally leaves the corpus for the caller
            // (for example tests/distill_realcodex.py) to consume.
            prep.commit_corpus();
            Ok(())
        }
        Some(other) => {
            bail!("unknown argument: {other}");
        }
        None => ui::run_manager(),
    }
}

fn print_help() {
    println!(
        "rail - Codex Rail\n\n\
         Run `rail` to open the Codex session manager.\n\n\
         Hidden worker mode is used internally by rail."
    );
}
