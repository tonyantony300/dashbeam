use clap::Parser;
use dashbeam_cli::commands::{self, Cli};
use std::io::Write;

fn main() {
    // Before any thread exists: env mutation is not thread-safe, and the agent
    // must never honor IROH_SECRET (nor pass it to an auto-started node).
    let iroh_secret_was_set = std::env::var_os("IROH_SECRET").is_some();
    if iroh_secret_was_set {
        std::env::remove_var("IROH_SECRET");
    }
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("dashbeam: start runtime: {e}");
            std::process::exit(1);
        }
    };
    let code = match runtime.block_on(commands::run(cli, iroh_secret_was_set)) {
        Ok(code) => code,
        Err(e) => {
            // Errors can quote peer-chosen names (e.g. ambiguous device lists).
            eprintln!(
                "dashbeam: {}",
                dashbeam_cli::prompt::display_safe_multiline(&format!("{e:#}"))
            );
            1
        }
    };
    let _ = std::io::stdout().flush();
    // Don't wait on lingering blocking tasks (e.g. the stdin prompt reader).
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    std::process::exit(code);
}
