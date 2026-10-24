//! The headless PS1 the host drives: one `Cpu` + `Bus`, the editor's route-tick
//! frame clock, persistent controller overrides, input-tape playback and
//! save states.
//!
//! Everything here runs on the emulator thread. A [`Machine`] is only ever
//! touched between instructions it executed itself, so every observation the
//! host makes is taken at an exact instruction boundary -- there is no second
//! thread racing the core.
//!
//! ## Frame clock
//! A *frame* is one route tick: a `vblank_period` cycle budget, capped at
//! [`ROUTE_STEPS_PER_FRAME`] instructions. That is exactly what
//! `app::step_one_frame` runs per GUI redraw and what the headless
//! `launch` command counts as `route-ticks`, so video-frame input tapes
//! recorded in the editor or the GUI replay here sample for sample.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use emulator_core::input_tape::{game_image_hash, GameImageHasher, PadSample, Tape, TapeClock};
use emulator_core::{fast_boot_disc, Bus, Cpu, EmulatorState, EmulatorStateRef};
use psoxide_settings::savestate::SaveStateV1;
use psx_iso::{Disc, Exe};
use serde_json::{json, Value};

/// Instruction cap per route tick (`AppState::run_steps_per_frame` default and
/// the headless CLI's `ROUTE_RUN_STEPS_PER_FRAME`).
pub const ROUTE_STEPS_PER_FRAME: u64 = 1_000_000;

/// Frames an `input_polls` advance may run per requested poll before it reports
/// that the guest stopped polling, instead of spinning until the host deadline.
pub const POLL_FRAME_BUDGET: u64 = 600;

/// Events retained for `poll_events` before the oldest is dropped.
const MAX_EVENTS: usize = 1024;

/// Stereo SPU samples (a quarter second at 44.1 kHz) a machine capturing audio
/// keeps for [`Machine::drain_audio`]. A longer backlog -- an agent advanced
/// many frames between two drains -- is dropped at the next frame end, so the
/// queue never grows without bound and never plays back seconds late.
pub const AUDIO_CAPTURE_SAMPLES: usize = 44_100 / 4;

/// How the port-1 controller is presented at boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PadProfile {
    /// DualShock forced into analog mode (poll ID 0x73). The headless `launch`
    /// default, so tapes recorded there replay identically.
    #[default]
    Analog,
    /// DualShock that powers up digital and switches on the guest's handshake,
    /// like the GUI's default controller (`launch --dualshock-pad`).
    DualShock,
    /// Original digital pad, poll ID 0x41 (`launch --digital-pad`).
    Digital,
}

/// What to boot.
#[derive(Debug, Clone)]
pub enum Content {
    /// A `.cue`, `.ccd`, `.bin`, `.iso` or PSX `.exe` on disk.
    File(PathBuf),
    /// An in-memory PSX-EXE, side-loaded like an `.exe` file (tests, tools).
    ExeBytes {
        /// Name reported as the content identity.
        label: String,
        /// The raw `PS-X EXE` image.
        bytes: Vec<u8>,
    },
}

/// Boot parameters. Kept so `reset` can reboot exactly the same machine.
#[derive(Debug, Clone)]
pub struct BootConfig {
    /// What to boot.
    pub content: Content,
    /// Port-1 controller profile.
    pub pad: PadProfile,
    /// Plug a digital pad into port 2 as well.
    pub pad2: bool,
    /// Auxiliary disc mounted beside a side-loaded EXE (`launch --disc`).
    pub disc: Option<PathBuf>,
    /// Apply the late PAL SCPH-9902 memory-controller timing profile.
    pub scph_9902: bool,
    /// Game id written into save-state headers (`launch` writes `headless`).
    pub game_id: String,
}

impl BootConfig {
    /// Defaults matching a plain headless `launch --path <content>`.
    pub fn new(content: Content) -> Self {
        Self {
            content,
            pad: PadProfile::default(),
            pad2: false,
            disc: None,
            scph_9902: false,
            game_id: "headless".into(),
        }
    }
}

/// The unit an advance is counted in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    /// Route ticks (see the module docs).
    Frames,
    /// Completed port-1 controller polls (`Bus::port1_completed_polls`).
    InputPolls,
    /// Retired CPU instructions, one `Cpu::step` each.
    Instructions,
}

impl Unit {
    /// Wire name of the unit.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Frames => "frames",
            Self::InputPolls => "input_polls",
            Self::Instructions => "instructions",
        }
    }
}

/// Host-side limits and cancellation for one advance.
pub struct AdvanceControl<'a> {
    /// Host wall-clock deadline; the advance stops at the boundary it reached.
    pub deadline: Instant,
    /// Set by the transport when the requesting connection goes away.
    pub cancel: Option<&'a AtomicBool>,
}

/// Why an advance ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// Exactly the requested count was completed.
    Completed,
    /// Stopped early at an instruction boundary; the reason is a wire token.
    Interrupted(&'static str),
}

/// Result of [`Machine::advance`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advance {
    /// Unit the advance counted.
    pub unit: Unit,
    /// Count asked for.
    pub requested: u64,
    /// Count actually completed (equal to `requested` when completed).
    pub completed: u64,
    /// How it ended.
    pub stop: Stop,
}

/// Extra bus condition a batch stops on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Watch {
    Nothing,
    /// The port-1 poll counter moved.
    Polls,
    /// The display start address left this value.
    DisplayStart((u16, u16)),
}

/// Where the machine is stopped right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boundary {
    /// Booted, no instruction executed yet.
    PreFirstInstruction,
    /// At the end of a route tick.
    Frame,
    /// Mid-tick, after an instruction or poll-bound advance.
    Instruction,
}

impl Boundary {
    /// Wire name of the boundary.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreFirstInstruction => "pre_first_instruction",
            Self::Frame => "frame_boundary",
            Self::Instruction => "instruction_boundary",
        }
    }
}

/// Persistent controller state for one port, owned by the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortInput {
    /// Pressed buttons, PSX bit layout (`emulator_core::button`), active-high.
    pub buttons: u16,
    /// `[right_x, right_y, left_x, left_y]`, 0x00..=0xff, 0x80 centred.
    pub sticks: [u8; 4],
    /// Whether the agent holds an override (false = released / neutral pad).
    pub engaged: bool,
}

impl Default for PortInput {
    fn default() -> Self {
        Self {
            buttons: 0,
            sticks: [0x80; 4],
            engaged: false,
        }
    }
}

impl PortInput {
    fn sample(self) -> PadSample {
        PadSample {
            buttons: self.buttons,
            right_x: self.sticks[0],
            right_y: self.sticks[1],
            left_x: self.sticks[2],
            left_y: self.sticks[3],
        }
    }
}

/// A PC (exec) breakpoint: stop before executing any instruction whose
/// address, with the segment bits masked off, lies in `start..=end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecBreakpoint {
    /// Public id.
    pub id: u64,
    /// First address as requested.
    pub start: u32,
    /// Last address (inclusive) as requested.
    pub end: u32,
    /// Times it stopped execution.
    pub hits: u64,
}

impl ExecBreakpoint {
    fn matches(&self, pc: u32) -> bool {
        let pc = pc & 0x1fff_ffff;
        (self.start & 0x1fff_ffff..=self.end & 0x1fff_ffff).contains(&pc)
    }
}

/// An input tape currently driving port 1.
#[derive(Debug, Clone)]
pub struct TapePlayback {
    /// The tape.
    pub tape: Tape,
    /// Video-frame tapes: route ticks to wait before sample 0.
    pub delay_ticks: u64,
    /// Frame counter value when playback started (video-frame tapes index from it).
    pub start_frame: u64,
    /// Index of the sample currently applied, if any.
    pub cursor: Option<usize>,
    /// Whether an advance should stop when the tape runs out.
    pub stop_on_end: bool,
    /// Where the tape came from, for status.
    pub source: String,
}

/// The emulated machine plus everything the host tracks about it.
pub struct Machine {
    config: BootConfig,
    cpu: Cpu,
    bus: Bus,
    frame: u64,
    tick_deadline: u64,
    tick_steps: u64,
    boundary: Boundary,
    inputs: [PortInput; 2],
    tape: Option<TapePlayback>,
    tape_seen_polls: u64,
    port1_sample: PadSample,
    port1_hold_neutral: bool,
    recording: bool,
    recorded: Vec<PadSample>,
    audio_capture: bool,
    game_hash: u64,
    poll_offset: u64,
    exact_poll_stepping: bool,
    halted: Option<String>,
    breakpoints: Vec<ExecBreakpoint>,
    next_breakpoint_id: u64,
    hit_seq: u64,
    resume_from_hit: Option<u32>,
    last_hit: Option<Value>,
    events: VecDeque<Value>,
    dropped_events: u64,
    event_seq: u64,
    stop_epoch: u64,
    mapping_epoch: u64,
    reset_seq: u64,
}

impl Machine {
    /// Boot `config` and stop before the first instruction.
    pub fn boot(config: BootConfig) -> Result<Self, String> {
        let (cpu, bus, game_hash) = boot_core(&config)?;
        let mut machine = Self {
            config,
            cpu,
            bus,
            frame: 0,
            tick_deadline: 0,
            tick_steps: 0,
            boundary: Boundary::PreFirstInstruction,
            inputs: [PortInput::default(); 2],
            tape: None,
            tape_seen_polls: 0,
            port1_sample: PadSample::default(),
            port1_hold_neutral: false,
            recording: false,
            recorded: Vec::new(),
            audio_capture: false,
            game_hash,
            poll_offset: 0,
            exact_poll_stepping: false,
            halted: None,
            breakpoints: Vec::new(),
            next_breakpoint_id: 1,
            hit_seq: 0,
            resume_from_hit: None,
            last_hit: None,
            events: VecDeque::new(),
            dropped_events: 0,
            event_seq: 0,
            stop_epoch: 0,
            mapping_epoch: 1,
            reset_seq: 0,
        };
        machine.restart_tick_clock();
        machine.publish_input(0);
        machine.publish_input(1);
        Ok(machine)
    }

    /// Boot configuration (content, pad profile, ...).
    pub fn config(&self) -> &BootConfig {
        &self.config
    }

    /// The CPU, for read-only observation.
    pub fn cpu(&self) -> &Cpu {
        &self.cpu
    }

    /// The bus, for read-only observation.
    pub fn bus(&self) -> &Bus {
        &self.bus
    }

    /// Completed route ticks since boot. Monotonic for the life of the process:
    /// neither `reset` nor `load_state` rewinds it.
    pub fn frame(&self) -> u64 {
        self.frame
    }

    /// Completed port-1 controller polls since boot, monotonic across `reset`
    /// and `load_state`.
    pub fn input_polls(&self) -> u64 {
        self.poll_offset + self.bus.port1_completed_polls()
    }

    /// The pad's own poll counter (`Bus::port1_completed_polls`), the PXITAPE2
    /// clock. PSoXide keeps it out of save states, so it restarts at zero on
    /// `load_state` and `reset`, exactly as after `launch --savestate`.
    pub fn guest_pad_polls(&self) -> u64 {
        self.bus.port1_completed_polls()
    }

    /// Single-step the final poll interval of every `input_polls` advance
    /// instead of trusting the batch stop check. Slower; a cross-check of the
    /// batch path, which must stop on the same instruction.
    pub fn set_exact_poll_stepping(&mut self, on: bool) {
        self.exact_poll_stepping = on;
    }

    /// Where the machine currently stands.
    pub fn boundary(&self) -> Boundary {
        self.boundary
    }

    /// Execution error that halted the CPU, if any.
    pub fn halted(&self) -> Option<&str> {
        self.halted.as_deref()
    }

    /// Advances that ended since boot; part of the memory-batch boundary token.
    pub fn stop_epoch(&self) -> u64 {
        self.stop_epoch
    }

    /// Bumped whenever the machine image is replaced (`reset`, `load_state`).
    pub fn mapping_epoch(&self) -> u64 {
        self.mapping_epoch
    }

    /// Persistent override of one port.
    pub fn port_input(&self, port: usize) -> PortInput {
        self.inputs[port]
    }

    /// The active input tape, if any.
    pub fn tape(&self) -> Option<&TapePlayback> {
        self.tape.as_ref()
    }

    /// [`game_image_hash`] of the booted image: the EXE file, or every disc
    /// track's raw bytes in order (the GUI's input-tape image hash).
    pub fn game_hash(&self) -> u64 {
        self.game_hash
    }

    /// Whether port 2 has a controller to drive.
    pub fn port2_connected(&self) -> bool {
        self.bus.port2_pad_mode().is_some()
    }

    fn restart_tick_clock(&mut self) {
        self.tick_deadline = self
            .bus
            .cycles()
            .saturating_add(self.bus.vblank_period().max(1));
        self.tick_steps = 0;
    }

    // --- input -------------------------------------------------------------

    /// Replace the persistent override of `port` (0 or 1). `None` releases it
    /// back to a neutral pad.
    pub fn set_port_input(&mut self, port: usize, input: Option<PortInput>) {
        self.inputs[port] = match input {
            Some(input) => PortInput {
                engaged: true,
                ..input
            },
            None => PortInput::default(),
        };
        self.publish_input(port);
    }

    /// Write the effective state of `port` to the bus: the tape on port 1 while
    /// one is playing, otherwise the agent's override (or a neutral pad), or a
    /// neutral pad regardless while [`Machine::set_port1_hold_neutral`] holds.
    fn publish_input(&mut self, port: usize) {
        if port == 0 {
            if let Some(sample) = self.tape_sample() {
                self.apply_port1(sample);
            } else if self.port1_hold_neutral {
                self.apply_port1(PortInput::default().sample());
            } else {
                self.apply_port1(self.inputs[0].sample());
            }
        } else if self.port2_connected() {
            self.inputs[1].sample().apply_to_bus_port2(&mut self.bus);
        }
    }

    fn apply_port1(&mut self, sample: PadSample) {
        self.port1_sample = sample;
        sample.apply_to_bus(&mut self.bus);
    }

    /// While `hold` is set, port 1 reads a neutral pad whenever no tape sample
    /// applies, whatever the agent's override says. A replay holds it so polls
    /// past the end of a short tape see what a recording would have seen
    /// without input, not live input. Clearing it re-applies the override.
    pub fn set_port1_hold_neutral(&mut self, hold: bool) {
        self.port1_hold_neutral = hold;
        self.publish_input(0);
    }

    fn tape_sample(&self) -> Option<PadSample> {
        let tape = self.tape.as_ref()?;
        tape.cursor.map(|index| tape.tape.samples[index])
    }

    // --- input tapes ---------------------------------------------------------

    /// Start replaying `tape` on port 1. Video-frame tapes start at the current
    /// frame (after `delay_ticks`); poll-bound tapes key to the absolute port-1
    /// poll count, so sample N reaches poll `start_poll + N` whatever the frame
    /// rate -- the same clocks `launch --input-tape` uses.
    pub fn start_tape(
        &mut self,
        tape: Tape,
        delay_ticks: u64,
        stop_on_end: bool,
        source: String,
    ) -> Result<(), String> {
        if tape.samples.is_empty() {
            return Err("input tape has no samples".into());
        }
        if tape.clock == TapeClock::PadPoll && delay_ticks != 0 {
            return Err("delay_ticks applies to video-frame tapes only".into());
        }
        self.tape = Some(TapePlayback {
            tape,
            delay_ticks,
            start_frame: self.frame,
            cursor: None,
            stop_on_end,
            source,
        });
        self.tape_seen_polls = self.bus.port1_completed_polls();
        self.sync_tape(true);
        Ok(())
    }

    /// Stop the tape and hand port 1 back to the agent's override.
    pub fn stop_tape(&mut self) -> Option<TapePlayback> {
        let tape = self.tape.take();
        self.publish_input(0);
        tape
    }

    /// Advance the tape cursor to the current clock. Returns true when the tape
    /// just ran out (the tape is then finished and port 1 released).
    fn sync_tape(&mut self, force: bool) -> bool {
        let Some(playback) = self.tape.as_mut() else {
            return false;
        };
        let len = playback.tape.samples.len() as u64;
        let index = match playback.tape.clock {
            TapeClock::VideoFrame => {
                let elapsed = self.frame - playback.start_frame;
                if elapsed < playback.delay_ticks {
                    return false;
                }
                elapsed - playback.delay_ticks
            }
            TapeClock::PadPoll => {
                let polls = self.bus.port1_completed_polls();
                if !force && polls == self.tape_seen_polls {
                    return false;
                }
                self.tape_seen_polls = polls;
                if polls < playback.tape.start_poll {
                    return false;
                }
                polls - playback.tape.start_poll
            }
        };
        if index >= len {
            let finished = self.tape.take().expect("checked above");
            self.push_event(json!({
                "type": "input_tape_finished",
                "source": finished.source,
                "samples": len,
                "frame": self.frame,
                "guest_pad_polls": self.bus.port1_completed_polls(),
            }));
            self.publish_input(0);
            return finished.stop_on_end;
        }
        let index = index as usize;
        if playback.cursor != Some(index) {
            playback.cursor = Some(index);
            let sample = playback.tape.samples[index];
            self.apply_port1(sample);
        }
        false
    }

    // --- recording -----------------------------------------------------------

    /// Start recording port 1 from scratch: from now on every completed
    /// port-1 poll appends the sample the guest read at it, whether a tape or
    /// the override supplied it. Call it at boot so entry N is poll N, the
    /// clock a poll-bound tape with `start_poll` 0 replays on. A `reset` or
    /// `load_state` restarts the guest's poll clock but not the recording, so
    /// a recording spanning one no longer lines up with a replay from boot.
    pub fn start_recording(&mut self) {
        self.recording = true;
        self.recorded.clear();
    }

    /// Samples recorded since [`Machine::start_recording`], one per completed
    /// port-1 poll; empty when not recording.
    pub fn recorded_samples(&self) -> &[PadSample] {
        &self.recorded
    }

    /// Append what the guest read at the `completed` polls that just finished.
    /// The button and stick bytes come from the pad's record of the poll
    /// transaction itself, so an input change that landed between two bytes
    /// of the poll is recorded as the mix the guest actually received; bytes
    /// the poll did not carry (the sticks of a digital poll) keep the sample
    /// published on port 1.
    fn record_polls(&mut self, completed: u64) {
        let mut sample = self.port1_sample;
        if let Some(poll) = self.bus.port1_recent_polls().last() {
            if poll.complete && poll.len >= 4 {
                sample.buttons = !(u16::from(poll.rx[2]) | (u16::from(poll.rx[3]) << 8));
            }
            if poll.complete && poll.len >= 8 {
                sample.right_x = poll.rx[4];
                sample.right_y = poll.rx[5];
                sample.left_x = poll.rx[6];
                sample.left_y = poll.rx[7];
            }
        }
        for _ in 0..completed {
            self.recorded.push(sample);
        }
    }

    // --- execution -----------------------------------------------------------

    /// Run until `count` units completed, the CPU faulted, the tape ended with
    /// `stop_on_end`, the request was cancelled, or the host deadline passed.
    /// Always ends at an instruction boundary with the machine stopped.
    pub fn advance(&mut self, unit: Unit, count: u64, control: &AdvanceControl<'_>) -> Advance {
        let frames_start = self.frame;
        let polls_start = self.bus.port1_completed_polls();
        let mut instructions = 0u64;
        let mut skip_hit_at = self.resume_from_hit.take();
        self.last_hit = None;
        let completed = |machine: &Self, instructions: u64| match unit {
            Unit::Frames => machine.frame - frames_start,
            Unit::InputPolls => machine
                .bus
                .port1_completed_polls()
                .saturating_sub(polls_start),
            Unit::Instructions => instructions,
        };
        let stop = loop {
            let done = completed(self, instructions);
            if done >= count {
                break Stop::Completed;
            }
            if self.halted.is_some() {
                break Stop::Interrupted("execution_error");
            }
            if control
                .cancel
                .is_some_and(|flag| flag.load(Ordering::Acquire))
            {
                break Stop::Interrupted("cancelled");
            }
            if Instant::now() >= control.deadline {
                break Stop::Interrupted("host_deadline");
            }
            if unit == Unit::InputPolls
                && self.frame - frames_start >= POLL_FRAME_BUDGET.saturating_mul(done + 1)
            {
                break Stop::Interrupted("no_input_poll");
            }
            let (max_steps, single) = match unit {
                Unit::Instructions => (1, true),
                // A poll completes synchronously on the guest's SIO0 DATA write,
                // and `Cpu::run` evaluates its stop check after every instruction
                // that can move the poll counter, so a batch stops on the
                // completing instruction. `exact_poll_stepping` re-checks that
                // claim by single-stepping the last poll interval instead.
                Unit::InputPolls => (
                    ROUTE_STEPS_PER_FRAME,
                    self.exact_poll_stepping && done + 1 >= count,
                ),
                Unit::Frames => (ROUTE_STEPS_PER_FRAME, false),
            };
            if self.breakpoint_hit(&mut skip_hit_at) {
                break Stop::Interrupted("breakpoint");
            }
            // With breakpoints armed every instruction is checked before it runs.
            let single = single || !self.breakpoints.is_empty();
            let watch = if unit == Unit::InputPolls {
                Watch::Polls
            } else {
                Watch::Nothing
            };
            let (ran, tape_ended) = self.run_batch(max_steps, single, watch);
            instructions += ran;
            if tape_ended {
                break Stop::Interrupted("input_tape_finished");
            }
        };
        let completed = completed(self, instructions);
        self.stop_epoch += 1;
        if instructions > 0 {
            self.boundary = if self.tick_steps == 0 {
                Boundary::Frame
            } else {
                Boundary::Instruction
            };
        }
        Advance {
            unit,
            requested: count,
            completed: completed.min(count),
            stop,
        }
    }

    /// Run until the display start address changes (the guest flipped its
    /// buffers), at most `max_frames` frames. Used after a poll-exact stop so
    /// the frame on screen is the one that poll's tick rendered
    /// (`launch --stop-at-poll` semantics).
    pub fn advance_to_display_flip(
        &mut self,
        max_frames: u64,
        control: &AdvanceControl<'_>,
    ) -> Stop {
        let start = self.display_start();
        let frames_start = self.frame;
        let mut skip_hit_at = self.resume_from_hit.take();
        self.last_hit = None;
        let stop = loop {
            if self.display_start() != start {
                break Stop::Completed;
            }
            if self.halted.is_some() {
                break Stop::Interrupted("execution_error");
            }
            if control
                .cancel
                .is_some_and(|flag| flag.load(Ordering::Acquire))
            {
                break Stop::Interrupted("cancelled");
            }
            if Instant::now() >= control.deadline {
                break Stop::Interrupted("host_deadline");
            }
            if self.frame - frames_start >= max_frames {
                break Stop::Interrupted("no_display_flip");
            }
            if self.breakpoint_hit(&mut skip_hit_at) {
                break Stop::Interrupted("breakpoint");
            }
            let single = !self.breakpoints.is_empty();
            if self
                .run_batch(ROUTE_STEPS_PER_FRAME, single, Watch::DisplayStart(start))
                .1
            {
                break Stop::Interrupted("input_tape_finished");
            }
        };
        self.stop_epoch += 1;
        self.boundary = if self.tick_steps == 0 {
            Boundary::Frame
        } else {
            Boundary::Instruction
        };
        stop
    }

    // --- breakpoints ---------------------------------------------------------

    /// Arm a PC breakpoint over `start..=end` (both in the same KUSEG/KSEG0/KSEG1
    /// view; segment bits are ignored when matching). Returns its id.
    pub fn add_breakpoint(&mut self, start: u32, end: u32) -> Result<u64, String> {
        if end < start {
            return Err("breakpoint end is below start".into());
        }
        if start >> 29 != end >> 29 {
            return Err("breakpoint range must stay inside one address segment".into());
        }
        if self.breakpoints.len() >= 256 {
            return Err("at most 256 breakpoints can be armed".into());
        }
        let id = self.next_breakpoint_id;
        self.next_breakpoint_id += 1;
        self.breakpoints.push(ExecBreakpoint {
            id,
            start,
            end,
            hits: 0,
        });
        Ok(id)
    }

    /// Disarm one breakpoint. Returns whether it existed.
    pub fn remove_breakpoint(&mut self, id: u64) -> bool {
        let before = self.breakpoints.len();
        self.breakpoints.retain(|bp| bp.id != id);
        self.breakpoints.len() != before
    }

    /// Disarm every breakpoint; returns how many were armed.
    pub fn clear_breakpoints(&mut self) -> usize {
        std::mem::take(&mut self.breakpoints).len()
    }

    /// Armed breakpoints.
    pub fn breakpoints(&self) -> &[ExecBreakpoint] {
        &self.breakpoints
    }

    /// The hit event that ended the last advance, if a breakpoint did.
    pub fn last_hit(&self) -> Option<&Value> {
        self.last_hit.as_ref()
    }

    /// Check the instruction about to run. The first check of an advance that
    /// resumes from a hit skips that same PC, so the stopped instruction runs.
    fn breakpoint_hit(&mut self, skip_at: &mut Option<u32>) -> bool {
        if self.breakpoints.is_empty() {
            return false;
        }
        let pc = self.cpu.pc();
        if skip_at.take() == Some(pc) {
            return false;
        }
        let Some(index) = self.breakpoints.iter().position(|bp| bp.matches(pc)) else {
            return false;
        };
        self.breakpoints[index].hits += 1;
        self.hit_seq += 1;
        let id = self.breakpoints[index].id;
        let mut regs = serde_json::Map::new();
        regs.insert("pc".into(), json!(pc));
        for index in 0..32u8 {
            regs.insert(format!("r{index}"), json!(self.cpu.gpr(index)));
        }
        let event = json!({
            "type": "breakpoint_hit",
            "id": id,
            "breakpoint_id": id,
            "kind": "exec",
            "address": pc,
            "pc": pc,
            "hit_seq": self.hit_seq,
            "frame": self.frame,
            "cpu_tick": self.cpu.tick(),
            "snapshot_timing": "pre_instruction",
            "regs": regs,
            "evidence_complete": true,
        });
        self.push_event(event.clone());
        self.last_hit = Some(event);
        self.resume_from_hit = Some(pc);
        true
    }

    fn display_start(&self) -> (u16, u16) {
        let area = self.bus.gpu.display_area();
        (area.x, area.y)
    }

    /// Execute one batch. Returns instructions retired and whether a tape with
    /// `stop_on_end` just ran out.
    fn run_batch(&mut self, max_steps: u64, single: bool, watch: Watch) -> (u64, bool) {
        let poll_tape = self
            .tape
            .as_ref()
            .is_some_and(|tape| tape.tape.clock == TapeClock::PadPoll);
        let seen = self.bus.port1_completed_polls();
        // Recording stops a batch on every completed poll too, so each poll's
        // sample is captured before the tape or the agent replaces it.
        let watch_polls = watch == Watch::Polls || poll_tape || self.recording;
        let display_start = match watch {
            Watch::DisplayStart(start) => Some(start),
            _ => None,
        };
        let budget = max_steps
            .min(ROUTE_STEPS_PER_FRAME - self.tick_steps)
            .max(1);
        let (ran, result) = if single || budget == 1 {
            match self.cpu.step(&mut self.bus) {
                Ok(()) => (1, Ok(())),
                Err(error) => (0, Err(error)),
            }
        } else {
            self.cpu
                .run(&mut self.bus, budget, self.tick_deadline, |bus| {
                    (watch_polls && bus.port1_completed_polls() != seen)
                        || display_start.is_some_and(|start| {
                            let area = bus.gpu.display_area();
                            (area.x, area.y) != start
                        })
                })
        };
        self.tick_steps += ran;
        if let Err(error) = result {
            let message = format!("{error:?}");
            self.push_event(json!({
                "type": "execution_error",
                "error": message,
                "pc": self.cpu.pc(),
                "frame": self.frame,
                "cpu_tick": self.cpu.tick(),
            }));
            self.halted = Some(message);
            return (ran, false);
        }
        let mut tape_ended = false;
        let polls = self.bus.port1_completed_polls();
        if self.recording && polls > seen {
            self.record_polls(polls - seen);
        }
        if poll_tape && polls != seen {
            tape_ended |= self.sync_tape(false);
        }
        if self.bus.cycles() >= self.tick_deadline || self.tick_steps >= ROUTE_STEPS_PER_FRAME {
            tape_ended |= self.finish_tick();
        }
        (ran, tape_ended)
    }

    fn finish_tick(&mut self) -> bool {
        self.frame += 1;
        self.restart_tick_clock();
        // Keep the SPU caught up and its host queue drained, as the GUI's
        // audio path does every frame; the guest never sees the drained samples.
        // A capturing machine leaves the queue to `drain_audio` up to its cap.
        self.bus.run_spu_to_current_cycle();
        let keep = if self.audio_capture {
            AUDIO_CAPTURE_SAMPLES
        } else {
            0
        };
        if self.bus.spu.audio_queue_len() > keep {
            let _ = self.bus.spu.drain_audio();
        }
        let video_tape = self
            .tape
            .as_ref()
            .is_some_and(|tape| tape.tape.clock == TapeClock::VideoFrame);
        video_tape && self.sync_tape(false)
    }

    // --- lifecycle -----------------------------------------------------------

    /// Reboot the same content from power-on. Persistent overrides survive and
    /// are re-applied; a playing tape is stopped.
    pub fn reset(&mut self) -> Result<(), String> {
        let (cpu, bus, _) = boot_core(&self.config)?;
        self.poll_offset += self.bus.port1_completed_polls();
        self.cpu = cpu;
        self.bus = bus;
        self.halted = None;
        self.resume_from_hit = None;
        self.reset_seq += 1;
        self.mapping_epoch += 1;
        self.stop_epoch += 1;
        self.boundary = Boundary::PreFirstInstruction;
        if let Some(tape) = self.tape.take() {
            self.push_event(
                json!({"type": "input_tape_stopped", "source": tape.source, "reason": "reset"}),
            );
        }
        self.restart_tick_clock();
        self.publish_input(0);
        self.publish_input(1);
        Ok(())
    }

    /// Times `reset` succeeded.
    pub fn reset_seq(&self) -> u64 {
        self.reset_seq
    }

    /// Serialize the machine as a PSoXide `SaveStateV1` (the GUI and
    /// `launch --savestate` format: BIOS and disc are not included).
    pub fn save_state_bytes(&self) -> Result<Vec<u8>, String> {
        let cpu = &self.cpu;
        let bus = &self.bus;
        let game_id = self.config.game_id.clone();
        on_big_stack(move || {
            SaveStateV1::new(EmulatorStateRef { cpu, bus }, game_id, cpu.tick())
                .to_bytes()
                .map_err(|error| error.to_string())
        })?
    }

    /// Restore a `SaveStateV1`. The disc and BIOS are carried over from the
    /// running machine, exactly as the GUI and the headless CLI restore states,
    /// so a state only makes sense for the content currently mounted. Returns
    /// the game id stored in the state header.
    pub fn load_state_bytes(&mut self, bytes: &[u8]) -> Result<String, String> {
        let loaded = on_big_stack(|| {
            SaveStateV1::<EmulatorState>::from_bytes(bytes).map_err(|error| error.to_string())
        })??;
        let mut payload = loaded.payload;
        payload.bus.restore_excluded_from(&mut self.bus);
        self.poll_offset += self.bus.port1_completed_polls();
        self.cpu = payload.cpu;
        self.bus = payload.bus;
        self.halted = None;
        self.resume_from_hit = None;
        self.mapping_epoch += 1;
        self.stop_epoch += 1;
        self.boundary = Boundary::Frame;
        self.restart_tick_clock();
        // Input belongs to the agent, not to the snapshot: a state captured
        // while a key was held must not keep it held here.
        self.tape_seen_polls = self.bus.port1_completed_polls();
        self.publish_input(0);
        self.publish_input(1);
        Ok(loaded.header.game_id)
    }

    // --- observation ---------------------------------------------------------

    /// FNV-1a-64 over all 1024x512 VRAM words, little-endian -- the
    /// `launch --dump-hash` `vram_fnv1a_64` value.
    pub fn vram_hash(&self) -> u64 {
        let mut hash = psx_hw::hash::Fnv1a64::new();
        for &word in self.bus.gpu.vram.words() {
            hash.update(&word.to_le_bytes());
        }
        hash.finish()
    }

    /// `(hash, width, height, byte_len)` of the visible display -- the
    /// `launch --dump-hash` / `--visual-hash-log` `display_fnv1a_64` value.
    pub fn display_hash(&self) -> (u64, u32, u32, usize) {
        self.bus.gpu.display_hash()
    }

    // --- audio --------------------------------------------------------------

    /// Keep the SPU's host output for [`Machine::drain_audio`] instead of
    /// dropping it at every frame end: for a frontend with an audio device.
    /// At most [`AUDIO_CAPTURE_SAMPLES`] are kept between two drains.
    pub fn set_audio_capture(&mut self, on: bool) {
        self.audio_capture = on;
    }

    /// Catch the SPU up to the current cycle and take its queued host output,
    /// as the GUI's audio path does after every frame. Host-side only: the
    /// guest never sees what is drained.
    pub fn drain_audio(&mut self) -> Vec<(i16, i16)> {
        self.bus.run_spu_to_current_cycle();
        self.bus.spu.drain_audio()
    }

    /// Mutable bus access for memory writes.
    pub(crate) fn bus_mut(&mut self) -> &mut Bus {
        &mut self.bus
    }

    // --- events --------------------------------------------------------------

    pub(crate) fn push_event(&mut self, mut event: Value) {
        self.event_seq += 1;
        if let Some(object) = event.as_object_mut() {
            object.insert("seq".into(), json!(self.event_seq));
        }
        if self.events.len() == MAX_EVENTS {
            self.events.pop_front();
            self.dropped_events += 1;
        }
        self.events.push_back(event);
    }

    /// Drain every pending event once (`EVENT.ONCE.1`).
    pub fn drain_events(&mut self) -> (Vec<Value>, u64) {
        let dropped = std::mem::take(&mut self.dropped_events);
        (self.events.drain(..).collect(), dropped)
    }
}

/// Run `f` on a thread with a 64 MiB stack. The derive-generated (de)serialize
/// walk over the whole machine overflows a default stack (see the GUI's
/// `AppState::save_state`).
fn on_big_stack<T: Send>(f: impl FnOnce() -> T + Send) -> Result<T, String> {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("psx-emucap-state".into())
            .stack_size(64 * 1024 * 1024)
            .spawn_scoped(scope, f)
            .map_err(|error| format!("spawn save-state thread: {error}"))?
            .join()
            .map_err(|_| "save-state thread panicked".to_string())
    })
}

/// Build a fresh CPU + bus for `config`, the same way `launch --path` does,
/// and the [`game_image_hash`] of what it booted.
fn boot_core(config: &BootConfig) -> Result<(Cpu, Bus, u64), String> {
    let mut cpu = Cpu::new();
    let mut bus = Bus::new_without_bios();
    let (is_exe, game_hash) = match &config.content {
        Content::ExeBytes { bytes, .. } => {
            side_load_exe(&mut cpu, &mut bus, bytes)?;
            (true, game_image_hash(bytes))
        }
        Content::File(path) => {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase)
                .unwrap_or_default();
            match ext.as_str() {
                "exe" => {
                    let bytes = std::fs::read(path)
                        .map_err(|error| format!("read {}: {error}", path.display()))?;
                    side_load_exe(&mut cpu, &mut bus, &bytes)?;
                    (true, game_image_hash(&bytes))
                }
                "bin" | "iso" | "cue" | "ccd" => {
                    let disc = load_disc(path)?;
                    let game_hash = disc_image_hash(&disc);
                    fast_boot_disc(&mut bus, &mut cpu, &disc)
                        .map_err(|error| format!("boot {}: {error:?}", path.display()))?;
                    bus.cdrom.insert_disc(Some(disc));
                    if matches!(ext.as_str(), "cue" | "ccd") {
                        apply_libcrypt_sbi(&mut bus, path);
                    }
                    (false, game_hash)
                }
                other => return Err(format!("unsupported content extension: .{other}")),
            }
        }
    };
    match config.pad {
        PadProfile::Digital => bus.attach_original_digital_pad_port1(),
        PadProfile::DualShock => bus.attach_digital_pad_port1(),
        PadProfile::Analog => {
            bus.attach_digital_pad_port1();
            let _ = bus.force_port1_analog_mode();
        }
    }
    if config.pad2 {
        bus.attach_digital_pad_port2();
    }
    if config.scph_9902 {
        bus.apply_scph_9902_profile();
    }
    if let Some(path) = config.disc.as_ref() {
        if !is_exe {
            return Err("an auxiliary disc can only be mounted beside an .exe".into());
        }
        bus.cdrom.insert_disc(Some(load_disc(path)?));
    }
    Ok((cpu, bus, game_hash))
}

/// The GUI's input-tape image hash for a disc (`app::disc_image_hash`): every
/// track's raw bytes in order, hashed as one stream, read a megabyte at a time.
fn disc_image_hash(disc: &Disc) -> u64 {
    let mut hasher = GameImageHasher::new();
    for track in disc.tracks() {
        psoxide_settings::disc_image::for_each_chunk(&*track.source, |bytes| hasher.update(bytes));
    }
    hasher.finish()
}

fn side_load_exe(cpu: &mut Cpu, bus: &mut Bus, bytes: &[u8]) -> Result<(), String> {
    let exe = Exe::parse(bytes).map_err(|error| format!("parse EXE: {error:?}"))?;
    bus.load_exe_payload(exe.load_addr, &exe.payload);
    bus.clear_exe_bss(exe.bss_addr, exe.bss_size);
    cpu.seed_from_exe(exe.initial_pc, exe.initial_gp, exe.initial_sp());
    // Side-loaded EXEs need the HLE syscall tables (same as the GUI and CLI).
    bus.enable_hle_bios();
    Ok(())
}

fn load_disc(path: &Path) -> Result<Disc, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "bin" | "iso" => psoxide_settings::library::load_disc_from_bin(path),
        "cue" => psoxide_settings::library::load_disc_from_cue(path),
        "ccd" => psoxide_settings::library::load_disc_from_ccd(path),
        other => Err(format!("unsupported disc extension: .{other}")),
    }
}

/// LibCrypt subchannel sectors from a sidecar `.sbi` (frontend `app::apply_libcrypt_sbi`).
fn apply_libcrypt_sbi(bus: &mut Bus, sheet: &Path) {
    match psoxide_settings::library::load_sbi_for(sheet) {
        Ok(Some(lbas)) => {
            eprintln!(
                "[emucap] LibCrypt: {} subchannel sectors from {}",
                lbas.len(),
                psoxide_settings::library::sbi_path_for(sheet).display()
            );
            bus.cdrom.set_bad_subq_sectors(lbas);
        }
        Ok(None) => {}
        Err(error) => eprintln!("[emucap] ignoring {error}"),
    }
}

/// Button name -> PSX pad bit. Names and aliases match emucap's Mednafen PSX
/// table, so agent prompts carry over between the two backends.
pub fn button_bit(name: &str) -> Option<u16> {
    use emulator_core::button::*;
    Some(
        match name.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "select" => SELECT,
            "l3" => L3,
            "r3" => R3,
            "start" | "enter" | "return" => START,
            "up" | "dpad_up" => UP,
            "right" | "dpad_right" => RIGHT,
            "down" | "dpad_down" => DOWN,
            "left" | "dpad_left" => LEFT,
            "l2" => L2,
            "r2" => R2,
            "l1" | "l" => L1,
            "r1" | "r" => R1,
            "triangle" => TRIANGLE,
            "circle" | "o" => CIRCLE,
            "cross" | "x" => CROSS,
            "square" => SQUARE,
            _ => return None,
        },
    )
}

/// Canonical names of the bits set in `mask`, in bit order.
pub fn button_names(mask: u16) -> Vec<&'static str> {
    const NAMES: [&str; 16] = [
        "select", "l3", "r3", "start", "up", "right", "down", "left", "l2", "r2", "l1", "r1",
        "triangle", "circle", "cross", "square",
    ];
    (0..16)
        .filter(|bit| mask & (1 << bit) != 0)
        .map(|bit| NAMES[bit])
        .collect()
}
