#![cfg(unix)]

mod attach;
mod distill;
mod progress;
mod protocol;
mod state;
mod ui;
mod worker;

use anyhow::{bail, Result};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);

    match args.next().as_deref() {
        Some("--worker") => {
            let Some(id) = args.next() else {
                bail!("missing worker session id");
            };
            worker::run_worker(&id)
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
        // Diagnostic/headless: aggregate the archive corpus without launching a
        // codex session, and print the plan (used by tests and for timing).
        Some("--distill-prepare") => {
            let prep = distill::prepare()?;
            // Also drop the exact prompt the UI would launch codex with, so a
            // headless/real-codex test can drive the same thing.
            let prompt_path = prep.workdir.join(".last-distill-prompt.txt");
            let _ = std::fs::write(&prompt_path, distill::distill_prompt(&prep));
            println!(
                "distill: {} sessions ({} codex + {} claude) · {} user turns · {} chunk(s) · scanned {} files, {} rich sessions available -> {}/corpus",
                prep.sessions,
                prep.codex_sessions,
                prep.claude_sessions,
                prep.messages,
                prep.chunks.len(),
                prep.scanned,
                prep.available,
                prep.workdir.display()
            );
            println!("next output: {}/{}", prep.workdir.display(), prep.output_file);
            println!("prompt: {}", prompt_path.display());
            for c in &prep.chunks {
                println!("  {} id={}", c.file, c.marker);
            }
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
