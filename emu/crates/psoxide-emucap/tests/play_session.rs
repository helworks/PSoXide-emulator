//! Play sessions against a real `Bus` + `Cpu`: record a run to one tape, replay
//! it to its recorded end on the next boot, and report whether the screen there
//! matches.
//!
//! The guest is the hand-assembled drawing guest from `common`: it polls the
//! port-1 pad, folds every polled word into a running hash in RAM and shows that
//! hash in the top-left display pixels, so the displayed frame depends on every
//! input the guest ever read.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use emulator_core::button::{CIRCLE, CROSS, START};
use emulator_core::input_tape::{read_tape_full, tape_poll_bound_bytes};
use psoxide_emucap::machine::{
    AdvanceControl, BootConfig, Content, Machine, PadProfile, PortInput, Stop, Unit,
};
use psoxide_emucap::play_session::{PlaySession, ReplayReport, ReplayResult};

use common::{at, drawing_guest_exe, COUNTER, INPUT_HASH, WIRE};

/// Frames the recorded session runs.
const SESSION_FRAMES: u64 = 120;

/// A scratch directory removed again when the test ends.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("psoxide-emucap-play-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn tape(&self) -> PathBuf {
        self.0.join("session.pxitape")
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn boot(exe: Vec<u8>) -> Machine {
    let mut config = BootConfig::new(Content::ExeBytes {
        label: "drawing-guest.exe".into(),
        bytes: exe,
    });
    config.pad = PadProfile::Digital;
    Machine::boot(config).expect("boot synthetic EXE")
}

fn run_frame(machine: &mut Machine) {
    let control = AdvanceControl {
        deadline: Instant::now() + Duration::from_secs(30),
        cancel: None,
    };
    let advance = machine.advance(Unit::Frames, 1, &control);
    assert_eq!(advance.stop, Stop::Completed);
}

fn ram_u32(machine: &Machine, offset: u64) -> u32 {
    let at = offset as usize;
    u32::from_le_bytes(machine.bus().ram()[at..at + 4].try_into().unwrap())
}

fn press(machine: &mut Machine, buttons: u16) {
    machine.set_port_input(
        0,
        Some(PortInput {
            buttons,
            ..PortInput::default()
        }),
    );
}

/// What a recorded run left behind at its end frame.
struct Recorded {
    input_hash: u32,
    polls: u32,
    frame: u64,
}

/// Boot `exe`, play [`SESSION_FRAMES`] frames holding Cross on frames 10-20,
/// and save the session to `path`.
fn record_session(path: &Path, exe: Vec<u8>) -> Recorded {
    let mut machine = boot(exe);
    let mut session = PlaySession::begin(&mut machine, path.to_path_buf());
    for frame in 0..SESSION_FRAMES {
        if frame == 10 {
            press(&mut machine, CROSS);
        } else if frame == 21 {
            machine.set_port_input(0, None);
        }
        run_frame(&mut machine);
        session.after_frame(&mut machine);
    }
    assert!(
        machine
            .recorded_samples()
            .iter()
            .any(|sample| sample.buttons == CROSS),
        "the guest never read the held Cross"
    );
    session.save(&machine).expect("save session");
    Recorded {
        input_hash: ram_u32(&machine, INPUT_HASH),
        polls: ram_u32(&machine, COUNTER),
        frame: machine.frame(),
    }
}

/// Run frames until the session reports, at most `limit` frames.
fn run_until_result(machine: &mut Machine, session: &mut PlaySession, limit: u64) -> ReplayResult {
    for _ in 0..limit {
        run_frame(machine);
        if let Some(result) = session.after_frame(machine) {
            return result;
        }
    }
    panic!("no replay result within {limit} frames");
}

#[test]
fn recorded_session_replays_to_same_state() {
    let dir = ScratchDir::new("same-state");
    let recorded = record_session(&dir.tape(), drawing_guest_exe());

    let mut machine = boot(drawing_guest_exe());
    let mut session = PlaySession::begin(&mut machine, dir.tape());
    assert!(session.replaying());
    let result = run_until_result(&mut machine, &mut session, 2 * SESSION_FRAMES);

    assert_eq!(result.report, ReplayReport::SameState);
    assert!(!result.image_changed);
    assert!(!session.replaying());
    assert_eq!(machine.frame(), recorded.frame);
    assert_eq!(ram_u32(&machine, INPUT_HASH), recorded.input_hash);
    assert_eq!(ram_u32(&machine, COUNTER), recorded.polls);
}

#[test]
fn changed_input_reports_diverged() {
    let dir = ScratchDir::new("diverged");
    record_session(&dir.tape(), drawing_guest_exe());
    let mut tape = read_tape_full(&dir.tape()).unwrap();
    tape.samples[1].buttons = CIRCLE;
    let bytes = tape_poll_bound_bytes(&tape.samples, 0, tape.end, tape.game_hash).unwrap();
    std::fs::write(dir.tape(), bytes).unwrap();

    let mut machine = boot(drawing_guest_exe());
    let mut session = PlaySession::begin(&mut machine, dir.tape());
    let result = run_until_result(&mut machine, &mut session, 2 * SESSION_FRAMES);

    match result.report {
        ReplayReport::Diverged { expected, actual } => assert_ne!(expected, actual),
        other => panic!("expected a divergence, got {other:?}"),
    }
    assert!(!result.image_changed);
}

#[test]
fn missing_tape_reports_no_tape_and_records() {
    let dir = ScratchDir::new("missing");
    let mut machine = boot(drawing_guest_exe());
    let mut session = PlaySession::begin(&mut machine, dir.tape());
    assert!(!session.replaying());

    run_frame(&mut machine);
    let first = session
        .after_frame(&mut machine)
        .expect("an immediate result");
    assert_eq!(first.report, ReplayReport::NoTape);
    assert!(!first.image_changed);
    for _ in 1..30 {
        run_frame(&mut machine);
        assert_eq!(session.after_frame(&mut machine), None);
    }
    session.save(&machine).unwrap();

    let bytes = std::fs::read(dir.tape()).unwrap();
    assert_eq!(&bytes[..8], b"PXITAPE3");
    let tape = read_tape_full(&dir.tape()).unwrap();
    let end = tape.end.expect("an end marker");
    assert_eq!(end.frame, 30);
    assert_eq!(end.display_hash, machine.display_hash().0);
    assert_eq!(tape.samples.len() as u64, machine.guest_pad_polls());
    assert!(!tape.samples.is_empty());
}

#[test]
fn corrupt_tape_reports_unreadable_and_starts_fresh() {
    let dir = ScratchDir::new("corrupt");
    std::fs::write(dir.tape(), b"garbage").unwrap();
    let mut machine = boot(drawing_guest_exe());
    let mut session = PlaySession::begin(&mut machine, dir.tape());
    assert!(!session.replaying());
    assert!(machine.tape().is_none());

    run_frame(&mut machine);
    let first = session
        .after_frame(&mut machine)
        .expect("an immediate result");
    assert!(
        matches!(first.report, ReplayReport::TapeUnreadable(_)),
        "{first:?}"
    );
    for _ in 1..30 {
        run_frame(&mut machine);
        assert_eq!(session.after_frame(&mut machine), None);
    }
    assert_eq!(
        machine.recorded_samples().len() as u64,
        machine.guest_pad_polls()
    );
    session.save(&machine).unwrap();
    assert_eq!(read_tape_full(&dir.tape()).unwrap().end.unwrap().frame, 30);
}

#[test]
fn short_tape_hands_over_after_end_frame() {
    let dir = ScratchDir::new("short");
    let recorded = record_session(&dir.tape(), drawing_guest_exe());
    let mut tape = read_tape_full(&dir.tape()).unwrap();
    let kept = tape.samples.len() / 2;
    tape.samples.truncate(kept);
    let bytes = tape_poll_bound_bytes(&tape.samples, 0, tape.end, tape.game_hash).unwrap();
    std::fs::write(dir.tape(), bytes).unwrap();

    let mut machine = boot(drawing_guest_exe());
    // A live input held from the start must not reach the guest before the
    // replay ends: polls past the tape's samples read the neutral pad.
    press(&mut machine, CROSS);
    let mut session = PlaySession::begin(&mut machine, dir.tape());
    let mut result = None;
    for _ in 0..2 * SESSION_FRAMES {
        run_frame(&mut machine);
        result = session.after_frame(&mut machine);
        if result.is_some() {
            break;
        }
        assert!(session.replaying());
        if machine.tape().is_none() {
            assert_eq!(ram_u32(&machine, WIRE) as u16, 0xffff, "neutral pad");
        }
    }
    let result = result.expect("a replay result at the end frame");
    assert_eq!(machine.frame(), recorded.frame);
    assert!(!session.replaying());
    assert!(machine.tape().is_none());
    // The dropped half held no buttons, and the neutral pad the guest read in
    // its place is exactly that.
    assert_eq!(result.report, ReplayReport::SameState);
    assert!(machine.recorded_samples()[kept..]
        .iter()
        .all(|sample| sample.buttons == 0));

    // Live input takes over after the end frame.
    for _ in 0..3 {
        run_frame(&mut machine);
        assert_eq!(session.after_frame(&mut machine), None);
    }
    assert_eq!(ram_u32(&machine, WIRE) as u16, !CROSS);
}

#[test]
fn save_is_atomic() {
    let dir = ScratchDir::new("atomic");
    let mut machine = boot(drawing_guest_exe());
    let mut session = PlaySession::begin(&mut machine, dir.tape());
    for _ in 0..10 {
        run_frame(&mut machine);
        session.after_frame(&mut machine);
    }
    session.save(&machine).unwrap();
    assert_eq!(session.frames_since_save(), 0);
    for _ in 0..10 {
        run_frame(&mut machine);
        session.after_frame(&mut machine);
    }
    assert_eq!(session.frames_since_save(), 10);
    // A second save replaces the first in place.
    session.save(&machine).unwrap();

    let mut tmp = dir.tape().into_os_string();
    tmp.push(".tmp");
    assert!(!Path::new(&tmp).exists());
    let tape = read_tape_full(&dir.tape()).unwrap();
    assert_eq!(tape.end.unwrap().frame, 20);
    let entries = std::fs::read_dir(&dir.0).unwrap().count();
    assert_eq!(entries, 1, "only the tape itself remains");
}

#[test]
fn different_image_sets_image_changed() {
    let dir = ScratchDir::new("image");
    record_session(&dir.tape(), drawing_guest_exe());

    let mut changed = drawing_guest_exe();
    // A header byte the loader ignores: the image differs, the program not.
    changed[0x7ff] ^= 0xff;
    let mut machine = boot(changed);
    let mut session = PlaySession::begin(&mut machine, dir.tape());
    let result = run_until_result(&mut machine, &mut session, 2 * SESSION_FRAMES);
    assert!(result.image_changed);
    assert_eq!(result.report, ReplayReport::SameState);
}

#[test]
fn input_change_inside_a_poll_records_what_the_guest_read() {
    let dir = ScratchDir::new("mid-poll");
    let mut machine = boot(drawing_guest_exe());
    let mut session = PlaySession::begin(&mut machine, dir.tape());
    press(&mut machine, START);
    for _ in 0..3 {
        run_frame(&mut machine);
        session.after_frame(&mut machine);
    }
    // Stop between the poll's two button bytes: the low byte (Start) is read,
    // the high byte (Cross) is not yet.
    machine.add_breakpoint(at(25), at(25)).unwrap();
    let control = AdvanceControl {
        deadline: Instant::now() + Duration::from_secs(30),
        cancel: None,
    };
    let advance = machine.advance(Unit::Frames, 1, &control);
    assert_eq!(advance.stop, Stop::Interrupted("breakpoint"));
    machine.clear_breakpoints();
    let poll = machine.guest_pad_polls() as usize;
    press(&mut machine, CROSS);
    for _ in 0..3 {
        run_frame(&mut machine);
        session.after_frame(&mut machine);
    }

    // The guest got Start from the old input and Cross from the new one.
    assert_eq!(machine.recorded_samples()[poll].buttons, START | CROSS);
    assert_eq!(machine.recorded_samples()[poll + 1].buttons, CROSS);
    assert_eq!(
        machine.recorded_samples().len() as u64,
        machine.guest_pad_polls()
    );
    session.save(&machine).unwrap();
    let recorded_hash = ram_u32(&machine, INPUT_HASH);

    let mut replay = boot(drawing_guest_exe());
    let mut session = PlaySession::begin(&mut replay, dir.tape());
    let result = run_until_result(&mut replay, &mut session, 20);
    assert_eq!(result.report, ReplayReport::SameState);
    assert_eq!(ram_u32(&replay, INPUT_HASH), recorded_hash);
}

#[test]
fn replay_result_event_names_the_report() {
    let diverged = ReplayResult {
        report: ReplayReport::Diverged {
            expected: 1,
            actual: 2,
        },
        image_changed: true,
    }
    .event();
    assert_eq!(diverged["type"], "play_replay_result");
    assert_eq!(diverged["report"], "diverged");
    assert_eq!(diverged["expected"], 1);
    assert_eq!(diverged["actual"], 2);
    assert_eq!(diverged["image_changed"], true);

    let unreadable = ReplayResult {
        report: ReplayReport::TapeUnreadable("bad magic".into()),
        image_changed: false,
    }
    .event();
    assert_eq!(unreadable["report"], "tape_unreadable");
    assert_eq!(unreadable["message"], "bad magic");
    assert!(unreadable["expected"].is_null());
}
