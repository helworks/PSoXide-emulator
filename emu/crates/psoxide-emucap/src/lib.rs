// SPDX-License-Identifier: GPL-2.0-or-later
//! In-process emucap host for PSoXide.
//!
//! emucap is a multi-emulator MCP: one Control MCP talks to
//! a *producer* per emulator over NDJSON/TCP. This crate makes PSoXide its own
//! producer, the way emucap's patched Mednafen, Flycast and Dolphin hosts are:
//! the wire is spoken from inside the emulator process, and every request runs
//! on the emulator thread between frames (the same model as the GUI's `mcp`
//! feature), so agents get PSoXide's exact clocks -- route-tick frames,
//! instruction steps, and stops on the Nth controller poll -- plus its
//! deterministic input tapes, VRAM/display hashes and save states.
//!
//! Layers, bottom up:
//! - [`wire`]: request/response framing.
//! - [`auth`]: the launch-bound `ECFA` producer authentication.
//! - [`machine`]: the headless `Cpu` + `Bus` and its clocks.
//! - [`memory`]: side-effect-free memory regions.
//! - [`host`]: method dispatch (`hello`, `status`, `step`, ...).
//! - [`play_session`]: record every session to one tape, replay it next time.
//! - [`transport`]: socket/stdio service and the emulator loop.
//!
//! [`run`] wires them together for the `psoxide-emucap-host` binary and the
//! frontend's `emucap-host` subcommand.

pub mod auth;
pub mod host;
pub mod machine;
pub mod memory;
pub mod play_session;
pub mod transport;
pub mod wire;

use std::path::PathBuf;
use std::sync::mpsc;

use clap::{Args, ValueEnum};

use host::{Host, HostIdentity};
use machine::{BootConfig, Content, Machine, PadProfile};
use play_session::PlaySession;
use transport::{serve_jobs, ProducerConfig};

/// Controller profile flag values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PadArg {
    /// DualShock forced to analog mode (headless `launch` default).
    Analog,
    /// DualShock that powers up digital, like the GUI default.
    Dualshock,
    /// Original digital pad.
    Digital,
}

/// Command-line arguments of the host.
#[derive(Debug, Clone, Args)]
pub struct HostArgs {
    /// Content to boot: `.cue`, `.ccd`, `.bin`, `.iso` or PSX `.exe`.
    #[arg(long)]
    pub path: PathBuf,
    /// Control MCP port. Defaults to `EMUCAP_PORT`, then 47800.
    #[arg(long)]
    pub port: Option<u16>,
    /// Serve NDJSON on stdin/stdout instead of connecting to emucap. No
    /// authentication; for local debugging and tests. The HLE BIOS prints guest
    /// TTY output to stdout too, so stdio clients must skip non-JSON lines.
    #[arg(long)]
    pub stdio: bool,
    /// Start frozen before the first instruction (also `EMUCAP_START_FROZEN=1`).
    #[arg(long)]
    pub start_frozen: bool,
    /// Free-run as fast as possible instead of at the console's frame rate.
    #[arg(long)]
    pub unthrottled: bool,
    /// Port-1 controller profile.
    #[arg(long, value_enum, default_value_t = PadArg::Analog)]
    pub pad: PadArg,
    /// Plug a digital pad into port 2 as well.
    #[arg(long)]
    pub pad2: bool,
    /// Mount a disc beside a side-loaded EXE.
    #[arg(long)]
    pub disc: Option<PathBuf>,
    /// Apply the PAL SCPH-9902 memory-controller timing profile.
    #[arg(long)]
    pub scph_9902: bool,
    /// Game id written into save-state headers.
    #[arg(long, default_value = "headless")]
    pub game_id: String,
    /// Replay this input tape on port 1 from boot (as `launch --input-tape`).
    #[arg(long)]
    pub input_tape: Option<PathBuf>,
    /// Route ticks to wait before a video-frame tape's first sample.
    #[arg(long, default_value_t = 0)]
    pub input_tape_delay_ticks: u64,
    /// Play-session tape: replay it from boot to its recorded end (emitting a
    /// `play_replay_result` event), record the whole session, and save it back
    /// here every 600 frames and on shutdown.
    #[arg(long, conflicts_with = "input_tape")]
    pub tape: Option<PathBuf>,
}

/// Free-running frames between periodic Play-session saves.
const PLAY_SAVE_FRAMES: u64 = 600;

/// Stack for the emulator thread. `Bus` is large and debug builds copy it by
/// value while booting; the 1 MiB Windows main-thread stack is not enough.
const EMULATOR_STACK_BYTES: usize = 64 * 1024 * 1024;

/// Boot the machine and serve emucap until the transport ends. The emulator
/// runs on its own large-stack thread; this call blocks until it exits.
pub fn run(args: HostArgs) -> Result<(), String> {
    std::thread::Builder::new()
        .name("psx-emucap-emu".into())
        .stack_size(EMULATOR_STACK_BYTES)
        .spawn(move || serve(args))
        .map_err(|error| format!("spawn emulator thread: {error}"))?
        .join()
        .map_err(|_| "emulator thread panicked".to_string())?
}

fn serve(args: HostArgs) -> Result<(), String> {
    let env = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    let mut config = BootConfig::new(Content::File(args.path.clone()));
    config.pad = match args.pad {
        PadArg::Analog => PadProfile::Analog,
        PadArg::Dualshock => PadProfile::DualShock,
        PadArg::Digital => PadProfile::Digital,
    };
    config.pad2 = args.pad2;
    config.disc = args.disc.clone();
    config.scph_9902 = args.scph_9902;
    config.game_id = args.game_id.clone();
    let mut machine = Machine::boot(config)?;
    if let Some(path) = args.input_tape.as_ref() {
        let tape = emulator_core::input_tape::read_tape_full(path)?;
        machine.start_tape(
            tape,
            args.input_tape_delay_ticks,
            false,
            path.display().to_string(),
        )?;
    }
    let mut session = args
        .tape
        .clone()
        .map(|path| PlaySession::begin(&mut machine, path));
    let identity = HostIdentity {
        launch_id: env("EMUCAP_LAUNCH_ID"),
        name: env("EMUCAP_NAME"),
        content: env("EMUCAP_CONTENT"),
        start_frozen: args.start_frozen || env("EMUCAP_START_FROZEN").as_deref() == Some("1"),
    };
    let mut host = Host::new(machine, identity.clone());
    let (jobs_tx, jobs_rx) = mpsc::channel();

    let transport = if args.stdio {
        std::thread::Builder::new()
            .name("psx-emucap-stdio".into())
            .spawn(move || {
                let stdin = std::io::stdin();
                let mut reader = stdin.lock();
                let mut writer = std::io::stdout();
                if let Err(error) =
                    transport::serve_lines(&mut reader, &mut writer, &jobs_tx, false)
                {
                    eprintln!("[emucap] stdio session failed: {error}");
                }
            })
    } else {
        let port = args
            .port
            .or_else(|| env("EMUCAP_PORT").and_then(|p| p.parse().ok()))
            .unwrap_or(47800);
        let producer = ProducerConfig {
            port,
            front_key: env("EMUCAP_SESSION_TOKEN")
                .ok_or("EMUCAP_SESSION_TOKEN is required to connect to emucap (or pass --stdio)")?,
            launch_id: identity
                .launch_id
                .clone()
                .ok_or("EMUCAP_LAUNCH_ID is required to connect to emucap (or pass --stdio)")?,
        };
        std::thread::Builder::new()
            .name("psx-emucap".into())
            .spawn(move || transport::run_producer(&producer, &jobs_tx))
    }
    .map_err(|error| format!("spawn transport thread: {error}"))?;

    let mut after_frame = |host: &mut Host| {
        let Some(session) = session.as_mut() else {
            return;
        };
        if let Some(result) = session.after_frame(host.machine_mut()) {
            host.machine_mut().push_event(result.event());
        }
        if session.frames_since_save() >= PLAY_SAVE_FRAMES {
            if let Err(error) = session.save(host.machine()) {
                eprintln!("[emucap] Play session save failed: {error}");
            }
        }
    };
    serve_jobs(&mut host, &jobs_rx, args.unthrottled, &mut after_frame);
    let _ = transport.join();
    if let Some(session) = session.as_ref() {
        session.save(host.machine())?;
    }
    Ok(())
}
