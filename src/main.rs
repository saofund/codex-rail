#![cfg(unix)]

mod attach;
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
            println!("rail {}", env!("CARGO_PKG_VERSION"));
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
