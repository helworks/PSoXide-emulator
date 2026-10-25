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
use emulator_core::input_tape::{
    read_tape_full, tape_poll_bound_bytes, tape_to_csv, write_tape, PadSample, TapeClock, TapeEnd,
};
use psoxide_emucap::host::{Host, HostIdentity};
use psoxide_emucap::machine::{
    AdvanceControl, BootConfig, Content, Machine, PadProfile, PortInput, Stop, Unit,
};
use psoxide_emucap::play_session::{PlaySession, ReplayReport, ReplayResult};
use psoxide_emucap::wire::Request;

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
    assert!(session.save(&machine).expect("save session"));
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
    let source = read_tape_full(&dir.tape()).unwrap();

    let mut machine = boot(drawing_guest_exe());
    let mut session = PlaySession::begin(&mut machine, dir.tape());
    assert!(session.replaying());
    let result = run_until_result(&mut machine, &mut session, 2 * SESSION_FRAMES);
    // The replay re-recorded exactly what it played.
    assert_eq!(machine.recorded_samples(), &source.samples[..]);

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
    assert!(session.save(&machine).unwrap());

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
    assert!(session.save(&machine).unwrap());
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
    assert!(session.save(&machine).unwrap());
    assert_eq!(session.frames_since_save(), 0);
    for _ in 0..10 {
        run_frame(&mut machine);
        session.after_frame(&mut machine);
    }
    assert_eq!(session.frames_since_save(), 10);
    // A second save replaces the first in place.
    assert!(session.save(&machine).unwrap());

    let mut tmp = dir.tape().into_os_string();
    tmp.push(".tmp");
    assert!(!Path::new(&tmp).exists());
    let tape = read_tape_full(&dir.tape()).unwrap();
    assert_eq!(tape.end.unwrap().frame, 20);
    let entries = std::fs::read_dir(&dir.0).unwrap().count();
    assert_eq!(entries, 1, "only the tape itself remains");
}

#[test]
fn save_during_replay_keeps_the_previous_tape() {
    let dir = ScratchDir::new("replay-save");
    record_session(&dir.tape(), drawing_guest_exe());
    let before = std::fs::read(dir.tape()).unwrap();

    let mut machine = boot(drawing_guest_exe());
    let mut session = PlaySession::begin(&mut machine, dir.tape());
    for _ in 0..SESSION_FRAMES / 2 {
        run_frame(&mut machine);
        assert_eq!(session.after_frame(&mut machine), None);
    }
    assert!(session.replaying());
    assert!(!session.can_save());
    // The flush and the shutdown save both go through `save`: mid-replay it
    // must leave the previous session's tape untouched.
    assert!(!session.save(&machine).unwrap());
    assert_eq!(std::fs::read(dir.tape()).unwrap(), before);

    run_until_result(&mut machine, &mut session, 2 * SESSION_FRAMES);
    for _ in 0..5 {
        run_frame(&mut machine);
        session.after_frame(&mut machine);
    }
    assert!(session.can_save());
    assert!(session.save(&machine).unwrap());
    let after = read_tape_full(&dir.tape()).unwrap();
    assert_eq!(after.end.unwrap().frame, SESSION_FRAMES + 5);
}

#[test]
fn unreadable_path_is_never_overwritten() {
    let dir = ScratchDir::new("io-error");
    // A directory where the tape should be: reading it fails with an I/O
    // error other than not-found, so it might be a tape the session cannot
    // see right now (locked, permissions) and must not be replaced.
    std::fs::create_dir_all(dir.tape()).unwrap();
    std::fs::write(dir.tape().join("marker"), b"keep").unwrap();
    let mut machine = boot(drawing_guest_exe());
    let mut session = PlaySession::begin(&mut machine, dir.tape());
    assert!(!session.replaying());

    run_frame(&mut machine);
    let first = session
        .after_frame(&mut machine)
        .expect("an immediate result");
    match &first.report {
        ReplayReport::TapeUnreadable(message) => assert!(message.starts_with("io:"), "{message}"),
        other => panic!("expected an unreadable tape, got {other:?}"),
    }
    for _ in 1..30 {
        run_frame(&mut machine);
        session.after_frame(&mut machine);
    }
    assert!(!session.can_save());
    assert!(!session.save(&machine).unwrap());
    assert!(dir.tape().is_dir());
    assert_eq!(std::fs::read(dir.tape().join("marker")).unwrap(), b"keep");
    let mut tmp = dir.tape().into_os_string();
    tmp.push(".tmp");
    assert!(!Path::new(&tmp).exists());
}

#[test]
fn non_play_tape_is_reported_and_never_overwritten() {
    let samples = [PadSample::from_buttons(CROSS), PadSample::from_buttons(0)];
    let end = Some(TapeEnd {
        frame: 30,
        display_hash: 1,
    });
    let pxitape1 = {
        let dir = ScratchDir::new("non-play-source");
        write_tape(&dir.tape(), &samples).unwrap();
        std::fs::read(dir.tape()).unwrap()
    };
    assert_eq!(&pxitape1[..8], b"PXITAPE1");
    let kinds: [(&str, Vec<u8>); 4] = [
        ("pxitape1", pxitape1),
        (
            "pxitape2",
            tape_poll_bound_bytes(&samples, 0, None, None).unwrap(),
        ),
        (
            "pxitape3-mid-run",
            tape_poll_bound_bytes(&samples, 5, end, None).unwrap(),
        ),
        (
            "csv",
            tape_to_csv(&samples, TapeClock::VideoFrame, 0, None).into_bytes(),
        ),
    ];
    for (kind, bytes) in kinds {
        let dir = ScratchDir::new(&format!("non-play-{kind}"));
        std::fs::write(dir.tape(), &bytes).unwrap();
        let mut machine = boot(drawing_guest_exe());
        let mut session = PlaySession::begin(&mut machine, dir.tape());
        assert!(!session.replaying(), "{kind}");

        run_frame(&mut machine);
        let first = session
            .after_frame(&mut machine)
            .expect("an immediate result");
        match &first.report {
            ReplayReport::TapeUnreadable(message) => assert!(
                message.contains("non-Play tape") && message.contains("will not be overwritten"),
                "{kind}: {message}"
            ),
            other => panic!("{kind}: expected an unreadable tape, got {other:?}"),
        }
        for _ in 1..30 {
            run_frame(&mut machine);
            session.after_frame(&mut machine);
        }
        assert!(!session.can_save(), "{kind}");
        assert!(!session.save(&machine).unwrap(), "{kind}");
        assert_eq!(std::fs::read(dir.tape()).unwrap(), bytes, "{kind}");
    }
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
    assert!(session.save(&machine).unwrap());
    let recorded_hash = ram_u32(&machine, INPUT_HASH);

    let mut replay = boot(drawing_guest_exe());
    let mut session = PlaySession::begin(&mut replay, dir.tape());
    let result = run_until_result(&mut replay, &mut session, 20);
    assert_eq!(result.report, ReplayReport::SameState);
    assert_eq!(ram_u32(&replay, INPUT_HASH), recorded_hash);
}

/// A frozen host on `exe` with a Play session on `tape`.
fn play_host(exe: Vec<u8>, tape: PathBuf) -> Host {
    let mut host = Host::new(
        boot(exe),
        HostIdentity {
            start_frozen: true,
            ..Default::default()
        },
    );
    host.begin_play_session(tape);
    host
}

fn step(host: &mut Host, method: &str, params: serde_json::Value) -> serde_json::Value {
    let line = serde_json::json!({"v": 1, "id": 7, "method": method, "params": params});
    let response = host.handle(&Request::parse(&line.to_string()).unwrap(), None);
    assert!(response.ok, "{method} failed: {:?}", response.error);
    response.result.unwrap()
}

#[test]
fn agent_step_across_the_end_frame_stops_there_and_hands_over() {
    let dir = ScratchDir::new("agent-step");
    let recorded = record_session(&dir.tape(), drawing_guest_exe());
    let mut host = play_host(drawing_guest_exe(), dir.tape());
    assert!(host.play_replaying());

    let before = step(&mut host, "step", serde_json::json!({"frames": 100}));
    assert_eq!(before["status"], "completed");
    assert!(before.get("play_replay_result").is_none());
    assert!(host.play_replaying());

    // 50 more frames would run 30 past the recorded end: the step stops at
    // the end frame and reports the hand-over check made there.
    let across = step(&mut host, "step", serde_json::json!({"frames": 50}));
    assert_eq!(across["status"], "interrupted", "{across}");
    assert_eq!(across["reason"], "play_replay_end");
    assert_eq!(across["frame"], recorded.frame);
    assert_eq!(across["completed"], recorded.frame - 100);
    assert_eq!(across["boundary"], "frame_boundary");
    assert_eq!(across["play_replay_result"]["report"], "same_state");
    assert!(!host.play_replaying());
    assert_eq!(
        host.replay_result().map(|result| &result.report),
        Some(&ReplayReport::SameState)
    );
    assert_eq!(ram_u32(host.machine(), INPUT_HASH), recorded.input_hash);
    let events = step(&mut host, "poll_events", serde_json::json!({}));
    let reported = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["type"] == "play_replay_result")
        .count();
    assert_eq!(reported, 1, "the hand-over is reported exactly once");

    // After the hand-over a step is unaffected.
    let after = step(&mut host, "step", serde_json::json!({"frames": 5}));
    assert_eq!(after["status"], "completed");
    assert_eq!(after["frame"], recorded.frame + 5);
    assert!(after.get("play_replay_result").is_none());
}

#[test]
fn poll_steps_and_free_frames_stop_at_the_end_frame_too() {
    let dir = ScratchDir::new("agent-polls");
    let recorded = record_session(&dir.tape(), drawing_guest_exe());

    // Poll steps: plenty of polls past the end frame are asked for.
    let mut host = play_host(drawing_guest_exe(), dir.tape());
    step(
        &mut host,
        "step",
        serde_json::json!({"frames": recorded.frame - 5}),
    );
    let polls = step(
        &mut host,
        "step_input_polls",
        serde_json::json!({"count": 100}),
    );
    assert_eq!(polls["status"], "interrupted", "{polls}");
    assert_eq!(polls["reason"], "play_replay_end");
    assert_eq!(polls["frame"], recorded.frame);
    assert_eq!(polls["play_replay_result"]["report"], "same_state");
    assert!(!host.play_replaying());
    // Past the hand-over the same request runs to completion.
    let polls = step(
        &mut host,
        "step_input_polls",
        serde_json::json!({"count": 3}),
    );
    assert_eq!(polls["status"], "completed");

    // Free-running frames get the same check through the host.
    let mut host = play_host(drawing_guest_exe(), dir.tape());
    step(&mut host, "resume", serde_json::json!({}));
    while host.play_replaying() {
        host.run_free_frame();
        assert!(host.machine().frame() <= recorded.frame);
    }
    assert_eq!(host.machine().frame(), recorded.frame);
    assert_eq!(
        host.replay_result().map(|result| &result.report),
        Some(&ReplayReport::SameState)
    );
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
