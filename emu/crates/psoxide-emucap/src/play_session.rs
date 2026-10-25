//! Play sessions: every session is recorded to one poll-bound tape, and the
//! next session replays that tape from boot to its recorded end before live
//! input takes over.
//!
//! A session's tape is a `PXITAPE3`: one sample per completed port-1 poll since
//! boot, plus the frame the session ended on and the display hash there. On the
//! next boot [`PlaySession::begin`] plays it back while recording again, and
//! [`PlaySession::after_frame`] reports, at the first frame boundary at or past
//! the recorded end frame, whether the display there is the recorded one. A
//! replay that does not reach the recorded state says so: the fast path never
//! silently resumes from a different state.
//!
//! Until the end frame port 1 is the tape's alone. Polls past the tape's last
//! sample read a neutral pad, exactly like a recording made without input, so a
//! tape that runs out early and one that is still playing at the end frame both
//! stop there and hand port 1 back to live input.

use std::cell::Cell;
use std::path::PathBuf;

use emulator_core::input_tape::{tape_from_bytes, tape_poll_bound_bytes, TapeClock, TapeEnd};
use serde_json::{json, Value};

use crate::machine::{Boundary, Machine};

/// How a replay ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayReport {
    /// No tape at the session path: nothing to replay, recording from boot.
    NoTape,
    /// The session path holds no usable tape and the session records from
    /// boot. Bytes that do not parse as any tape are replaced by the next
    /// save. A well-formed tape of another kind (`PXITAPE1`/`PXITAPE2`, CSV,
    /// or a `PXITAPE3` that does not start at poll 0) and a file that could not
    /// be read at all (an I/O error other than not-found; the message starts
    /// with `io:`) may hold a recording someone wants, so that session never
    /// saves over them.
    TapeUnreadable(String),
    /// The display at the end frame is the recorded one.
    SameState,
    /// The display at the end frame differs from the recorded one.
    Diverged {
        /// Display hash the tape recorded at its end frame.
        expected: u64,
        /// Display hash the replay reached there.
        actual: u64,
    },
}

impl ReplayReport {
    /// Wire name of the report.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NoTape => "no_tape",
            Self::TapeUnreadable(_) => "tape_unreadable",
            Self::SameState => "same_state",
            Self::Diverged { .. } => "diverged",
        }
    }
}

/// A replay's outcome plus whether the game image changed since the recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayResult {
    /// How the replay ended.
    pub report: ReplayReport,
    /// The tape was recorded against a different game image than the one
    /// booted. Advisory: the replay may still reach the same state.
    pub image_changed: bool,
}

impl ReplayResult {
    /// The `play_replay_result` event the host emits for this result.
    /// `expected` and `actual` are only set for a divergence, `message` only
    /// for an unreadable tape.
    pub fn event(&self) -> Value {
        let (expected, actual) = match self.report {
            ReplayReport::Diverged { expected, actual } => (json!(expected), json!(actual)),
            _ => (Value::Null, Value::Null),
        };
        let message = match &self.report {
            ReplayReport::TapeUnreadable(message) => json!(message),
            _ => Value::Null,
        };
        json!({
            "type": "play_replay_result",
            "report": self.report.as_str(),
            "expected": expected,
            "actual": actual,
            "image_changed": self.image_changed,
            "message": message,
        })
    }
}

/// One Play session on one machine, bound to the session's tape path.
pub struct PlaySession {
    path: PathBuf,
    /// The recorded end still to be reached, while replaying.
    end: Option<TapeEnd>,
    image_changed: bool,
    /// A result known at `begin` (no tape, unreadable tape), not yet returned.
    immediate: Option<ReplayResult>,
    /// The machine frame the last `after_frame` saw.
    frame: u64,
    /// The machine frame of the last save (or of `begin`).
    saved_frame: Cell<u64>,
    /// Why this session must never write its tape (the tape could not be
    /// read, or is a non-Play tape, so it may hold a recording the session
    /// must not destroy).
    save_refused: Option<String>,
}

impl PlaySession {
    /// Start a session on a freshly booted `machine`: read the tape at
    /// `tape_path` if there is one, replay it from boot when it holds a
    /// recorded end, and start recording either way.
    pub fn begin(machine: &mut Machine, tape_path: PathBuf) -> PlaySession {
        machine.start_recording();
        let mut session = PlaySession {
            path: tape_path,
            end: None,
            image_changed: false,
            immediate: None,
            frame: machine.frame(),
            saved_frame: Cell::new(machine.frame()),
            save_refused: None,
        };
        let bytes = match std::fs::read(&session.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                session.immediate = Some(ReplayResult {
                    report: ReplayReport::NoTape,
                    image_changed: false,
                });
                return session;
            }
            Err(error) => {
                let message = format!("io: read {}: {error}", session.path.display());
                session.save_refused = Some(message.clone());
                session.unreadable(message);
                return session;
            }
        };
        let tape = match tape_from_bytes(&bytes) {
            Ok(tape) => tape,
            Err(error) => {
                session.unreadable(format!("{}: {error}", session.path.display()));
                return session;
            }
        };
        let end = match tape.end {
            Some(end) if tape.clock == TapeClock::PadPoll && tape.start_poll == 0 => end,
            _ => {
                // A well-formed tape of another kind is somebody's recording:
                // never replace it with this session's.
                let message = format!(
                    "{}: a non-Play tape (a Play tape is poll-bound from boot with an end marker); it will not be overwritten",
                    session.path.display()
                );
                session.save_refused = Some(message.clone());
                session.unreadable(message);
                return session;
            }
        };
        session.image_changed = tape
            .game_hash
            .is_some_and(|hash| hash != machine.game_hash());
        session.end = Some(end);
        // No advance runs past the end frame before the hand-over check.
        machine.set_frame_stop(Some(end.frame));
        machine.set_port1_hold_neutral(true);
        if !tape.samples.is_empty() {
            let source = session.path.display().to_string();
            machine
                .start_tape(tape, 0, false, source)
                .expect("a poll-bound tape with samples and no delay always starts");
        }
        session
    }

    fn unreadable(&mut self, message: String) {
        self.immediate = Some(ReplayResult {
            report: ReplayReport::TapeUnreadable(message),
            image_changed: false,
        });
    }

    /// Whether the recorded end is still ahead: port 1 belongs to the tape and
    /// live input must not reach the guest yet.
    pub fn replaying(&self) -> bool {
        self.end.is_some()
    }

    /// Call after every advance. Returns the replay's result exactly once:
    /// right after `begin` when there was nothing to replay, otherwise at the
    /// frame boundary where the frame counter reaches the recorded end frame,
    /// where the tape is stopped, the display compared, and port 1 handed to
    /// live input. Until then the machine's frame stop holds every advance at
    /// that boundary, so it is never overrun. [`crate::host::Host`] calls it
    /// after each advance it runs, whoever asked for it.
    pub fn after_frame(&mut self, machine: &mut Machine) -> Option<ReplayResult> {
        self.frame = machine.frame();
        if let Some(result) = self.immediate.take() {
            return Some(result);
        }
        let end = self.end?;
        if machine.boundary() == Boundary::Instruction || machine.frame() < end.frame {
            return None;
        }
        self.end = None;
        machine.set_frame_stop(None);
        if machine
            .tape()
            .is_some_and(|tape| tape.source == self.source())
        {
            machine.stop_tape();
        }
        machine.set_port1_hold_neutral(false);
        let actual = machine.display_hash().0;
        let report = if actual == end.display_hash {
            ReplayReport::SameState
        } else {
            ReplayReport::Diverged {
                expected: end.display_hash,
                actual,
            }
        };
        Some(ReplayResult {
            report,
            image_changed: self.image_changed,
        })
    }

    fn source(&self) -> String {
        self.path.display().to_string()
    }

    /// Whether [`PlaySession::save`] would write now: not while replaying
    /// (the previous tape stays authoritative until the hand-over at its end
    /// frame), and never in a session whose tape could not be read or is a
    /// non-Play tape.
    pub fn can_save(&self) -> bool {
        !self.replaying() && self.save_refused.is_none()
    }

    /// Why this session never writes its tape, if it never does: the tape
    /// could not be read, or is a non-Play tape.
    pub fn save_refused(&self) -> Option<&str> {
        self.save_refused.as_deref()
    }

    /// Write the session so far as the session's tape: every recorded sample
    /// from poll 0, the current frame and display hash as its end, and the
    /// booted image's hash. Written to `<path>.tmp` and renamed over the tape,
    /// so an interrupted save leaves the previous tape intact.
    ///
    /// Returns `Ok(false)` without touching the file when
    /// [`PlaySession::can_save`] is false: a save during a replay would cut
    /// the previous session's tail off before it was replayed, and a tape
    /// that could not be read may still be a good one. `Ok(true)` means the
    /// tape was written.
    pub fn save(&self, machine: &Machine) -> Result<bool, String> {
        if !self.can_save() {
            return Ok(false);
        }
        let end = TapeEnd {
            frame: machine.frame(),
            display_hash: machine.display_hash().0,
        };
        let bytes = tape_poll_bound_bytes(
            machine.recorded_samples(),
            0,
            Some(end),
            Some(machine.game_hash()),
        )?;
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("mkdir {}: {error}", parent.display()))?;
            }
        }
        let mut tmp = self.path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        std::fs::write(&tmp, bytes).map_err(|error| format!("write {}: {error}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path).map_err(|error| {
            let _ = std::fs::remove_file(&tmp);
            format!(
                "replace {} with {}: {error}",
                self.path.display(),
                tmp.display()
            )
        })?;
        self.saved_frame.set(machine.frame());
        Ok(true)
    }

    /// Frames since the last save (or `begin`), as of the last `after_frame`;
    /// drives the periodic flush.
    pub fn frames_since_save(&self) -> u64 {
        self.frame.saturating_sub(self.saved_frame.get())
    }
}
