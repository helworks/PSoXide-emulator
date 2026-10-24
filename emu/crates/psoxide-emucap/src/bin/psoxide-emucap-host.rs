//! Headless PSoXide producer for emucap.
//!
//! `psoxide-emucap-host --path game.cue` boots the content and connects to the
//! Control MCP named by `EMUCAP_PORT` / `EMUCAP_SESSION_TOKEN` /
//! `EMUCAP_LAUNCH_ID`; `--stdio` serves the same NDJSON wire on stdin/stdout.

use clap::Parser;

/// Headless PSoXide emulator speaking emucap's control wire.
#[derive(Debug, Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(flatten)]
    host: psoxide_emucap::HostArgs,
}

fn main() {
    let cli = Cli::parse();
    if let Err(error) = psoxide_emucap::run(cli.host) {
        eprintln!("[emucap] {error}");
        std::process::exit(1);
    }
}
