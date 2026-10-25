//! `play-session`: a window driven by psoxide-emucap's Play core.
//!
//! The emulation is psoxide-emucap's `Host`/`Machine` -- the core emucap's
//! headless backend runs -- so recording, replay and agent control have one
//! set of semantics whether or not a window is open. Everything the session
//! decides (replay to the tape's end, the divergence check, recording, the
//! periodic flush) lives in [`psoxide_emucap::play_session`]; this module is
//! only the glue around it: window, live input, display and audio.
//!
//! Per redraw: drain queued emucap jobs (`--emucap`), then run guest frames
//! while the host is running. Until the replay hands over, frames run flat out
//! in slices of [`REPLAY_SLICE`] and port 1 belongs to the tape; afterwards
//! live port-1 input feeds the guest and frames are paced to its vblank
//! period, as the headless loop paces them. The display is presented once per
//! redraw. Closing the window saves the tape.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Args;
use emulator_core::pad::PadMode;
use emulator_core::Deinterlace;
use psoxide_emucap::host::{Host, HostIdentity, RunState};
use psoxide_emucap::machine::{BootConfig, Content, Machine, PortInput};
use psoxide_emucap::play_session::{ReplayReport, ReplayResult};
use psoxide_emucap::transport::{self, Job};
use psoxide_settings::settings::PortBindings;
use psoxide_settings::{ConfigPaths, Settings};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

use crate::gfx::Graphics;
use crate::playtest_input::Port1PadSample;
use crate::{
    audio, cpu_display_uv, guest_frame_dt, input, key_is_analog_button, key_to_pad_button,
    HostKeyboardInput,
};

/// Floating-window size with `--windowed`: a 4:3 frame at 3x 320x240.
const WINDOWED_WIDTH: u32 = 960;
const WINDOWED_HEIGHT: u32 = 720;

/// Wall time a redraw spends on unthrottled replay frames before presenting,
/// so a long replay still shows progress and the window stays responsive.
const REPLAY_SLICE: Duration = Duration::from_millis(33);

/// Paced frames one redraw may catch up on after a stall.
const MAX_CATCHUP_FRAMES: u32 = 4;

/// Lag behind the guest's frame clock past which pacing re-anchors instead
/// of racing to catch up (the headless loop's rule).
const MAX_PACING_LAG: Duration = Duration::from_millis(250);

/// Redraw cadence while the host is frozen: queued emucap jobs are drained
/// and frames an agent stepped are presented at this rate.
const FROZEN_TICK: Duration = Duration::from_millis(16);

/// Arguments for `play-session`.
#[derive(Debug, Args)]
pub struct PlaySessionArgs {
    /// Content to boot: `.cue`, `.ccd`, `.bin`, `.iso` or PSX `.exe`.
    #[arg(long)]
    pub disc: PathBuf,
    /// Play-session tape: replayed from boot to its recorded end, then the
    /// whole session is recorded back here every 600 frames and on close.
    #[arg(long)]
    pub tape: PathBuf,
    /// Connect to emucap as this launch's producer, with the managed-launch
    /// environment (`EMUCAP_PORT`, `EMUCAP_SESSION_TOKEN`, `EMUCAP_LAUNCH_ID`,
    /// `EMUCAP_NAME`, `EMUCAP_CONTENT`), so agents can observe and step the
    /// session.
    #[arg(long)]
    pub emucap: bool,
    /// Open a floating window instead of borderless fullscreen.
    #[arg(long)]
    pub windowed: bool,
}

/// Boot `args.disc`, start the Play session on `args.tape` and run the window
/// until it is closed. Returns the closing save's error, if it failed.
pub fn run(paths: &ConfigPaths, args: PlaySessionArgs) -> Result<(), String> {
    let settings = match Settings::load(&paths.settings_file()) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("[play-session] settings load: {error}; using defaults");
            Settings::default()
        }
    };
    // Without `--emucap` nothing could resume a frozen start, so the launch
    // environment is only read for an emucap session.
    let (identity, producer) = if args.emucap {
        let identity = psoxide_emucap::env_identity(false);
        let producer = psoxide_emucap::env_producer(None, &identity)?;
        (identity, Some(producer))
    } else {
        (HostIdentity::default(), None)
    };

    // The headless host's boot: HLE BIOS and its default (analog) pad profile.
    let disc = args.disc.clone();
    let tape = args.tape.clone();
    let host = psoxide_emucap::on_emulator_stack(move || {
        let mut machine = Machine::boot(BootConfig::new(Content::File(disc)))?;
        machine.set_audio_capture(true);
        let mut host = Box::new(Host::new(machine, identity));
        host.begin_play_session(tape);
        Ok::<_, String>(host)
    })??;

    let jobs = match producer {
        Some(producer) => {
            let (jobs_tx, jobs_rx) = mpsc::channel();
            std::thread::Builder::new()
                .name("psx-emucap".into())
                .spawn(move || transport::run_producer(&producer, &jobs_tx))
                .map_err(|error| format!("spawn transport thread: {error}"))?;
            Some(jobs_rx)
        }
        None => None,
    };

    let audio = audio::AudioOut::open();
    match audio.as_ref() {
        Some(audio) => eprintln!(
            "[audio] opened host stream @ {} Hz",
            audio.host_sample_rate()
        ),
        None => eprintln!("[audio] no host output device available - running silent"),
    }

    let event_loop = EventLoop::new().map_err(|error| format!("event loop: {error}"))?;
    let mut window = PlayWindow {
        host,
        replay_reported: false,
        title: String::new(),
        jobs,
        graphics: None,
        fullscreen: !args.windowed,
        bindings: settings.input.port1.clone(),
        deinterlace: Deinterlace::from_setting(&settings.video.deinterlace),
        host_input: HostKeyboardInput::default(),
        input: input::InputRouter::new(),
        audio,
        next_frame: Instant::now(),
        last_redraw: Instant::now(),
        presented_cycles: None,
        exit: Ok(()),
    };
    event_loop
        .run_app(&mut window)
        .map_err(|error| format!("event loop: {error}"))?;
    window.exit
}

/// The `[play-session] replay: ...` line printed when the replay ends.
fn replay_line(result: &ReplayResult) -> String {
    let report = match &result.report {
        // Decimal, as the `play_replay_result` event carries them.
        ReplayReport::Diverged { expected, actual } => {
            format!("diverged expected={expected} actual={actual}")
        }
        ReplayReport::TapeUnreadable(message) => format!("tape_unreadable: {message}"),
        report => report.as_str().to_string(),
    };
    format!(
        "[play-session] replay: {report} image_changed={}",
        result.image_changed
    )
}

/// The window title: what the Play session is doing, so the player sees the
/// replay's outcome without reading stderr.
fn window_title(
    replaying: bool,
    result: Option<&ReplayResult>,
    save_refused: Option<&str>,
    frozen: bool,
) -> String {
    let mut title = String::from("PSoXide Play");
    let status = if let Some(reason) = save_refused {
        Some(format!("tape not saved: {reason}"))
    } else if replaying {
        Some("replaying…".to_string())
    } else {
        result.map(|result| {
            let report = match &result.report {
                ReplayReport::SameState => "same state".to_string(),
                ReplayReport::Diverged { expected, actual } => {
                    format!("DIVERGED (expected {expected:016x}, got {actual:016x})")
                }
                ReplayReport::NoTape | ReplayReport::TapeUnreadable(_) => {
                    "new recording".to_string()
                }
            };
            if result.image_changed {
                format!("{report}, game image changed")
            } else {
                report
            }
        })
    };
    if let Some(status) = status {
        title.push_str(" — ");
        title.push_str(&status);
    }
    if frozen {
        title.push_str(" — frozen by agent");
    }
    title
}

/// The window and the Play session it drives.
struct PlayWindow {
    /// The emucap host owning the machine. Boxed: it was built on the
    /// emulator's large-stack thread and is never moved by value again.
    host: Box<Host>,
    /// Whether the replay result has been printed.
    replay_reported: bool,
    /// The window title last set.
    title: String,
    /// Jobs from the emucap transport (`--emucap`), until it goes away.
    jobs: Option<Receiver<Job>>,
    graphics: Option<Graphics>,
    fullscreen: bool,
    /// Port-1 keyboard bindings from the user's settings.
    bindings: PortBindings,
    deinterlace: Deinterlace,
    /// Port-1 pad state derived from keyboard events (as in the GUI shell).
    host_input: HostKeyboardInput,
    /// Keyboard and gamepads, routed to guest ports.
    input: input::InputRouter,
    audio: Option<audio::AudioOut>,
    /// When the next paced frame is due, once the replay has handed over.
    next_frame: Instant,
    last_redraw: Instant,
    /// Bus cycles at the last display upload; unchanged means the same frame.
    presented_cycles: Option<u64>,
    /// What `run` returns: the closing save's outcome.
    exit: Result<(), String>,
}

impl PlayWindow {
    /// Port 1's live state from the keyboard and every routed gamepad.
    fn live_port1(&mut self) -> PortInput {
        self.input.set_keyboard_held(input::PadSnapshot {
            buttons: self.host_input.resolved_mask(0),
            left: self.host_input.left_stick.vector(),
            right: self.host_input.right_stick.vector(),
            stick_dpad: 0,
            mode: false,
        });
        let pad_frame = self.input.poll();
        for notice in &pad_frame.notices {
            eprintln!("{}", notice.log_line());
        }
        // A pad in analog mode reads the stick as an axis; a digital pad has
        // no stick, so there the stick stands in for the D-pad.
        let analog = self.host.machine().bus().port1_pad_mode() == Some(PadMode::Analog);
        let port1 = pad_frame.port1;
        let sample =
            Port1PadSample::from_host(port1.mask_for(analog), port1.right_stick, port1.left_stick);
        PortInput {
            buttons: sample.buttons,
            sticks: [sample.right_x, sample.right_y, sample.left_x, sample.left_y],
            engaged: true,
        }
    }

    /// Run one guest frame: live input once the replay has handed over, the
    /// session's per-frame work, then the frame's audio. Audio replayed flat
    /// out is dropped rather than played back as a fast-forward smear.
    fn run_frame(&mut self, live: PortInput) {
        let replaying = self.host.play_replaying();
        if !replaying {
            self.host.machine_mut().set_port_input(0, Some(live));
        }
        self.host.run_free_frame();
        self.report_replay();
        let samples = self.host.machine_mut().drain_audio();
        if replaying || samples.is_empty() {
            return;
        }
        if let Some(audio) = self.audio.as_ref() {
            audio.push_samples(&samples);
        }
    }

    /// Print the replay result once the host has it, whether a free-running
    /// frame or an agent's step reached the hand-over.
    fn report_replay(&mut self) {
        if self.replay_reported {
            return;
        }
        if let Some(result) = self.host.replay_result() {
            eprintln!("{}", replay_line(result));
            self.replay_reported = true;
        }
    }

    /// The title for the session's current state.
    fn current_title(&self) -> String {
        window_title(
            self.host.play_replaying(),
            self.host.replay_result(),
            self.host
                .play_session()
                .and_then(|session| session.save_refused()),
            self.host.run_state() != RunState::Running,
        )
    }

    /// Set the window title when the session's state changed.
    fn update_title(&mut self) {
        let title = self.current_title();
        if title == self.title {
            return;
        }
        if let Some(gfx) = self.graphics.as_ref() {
            gfx.window.set_title(&title);
            self.title = title;
        }
    }

    /// Run this redraw's guest frames.
    fn advance(&mut self, live: PortInput) {
        if self.host.play_replaying() {
            let slice_end = Instant::now() + REPLAY_SLICE;
            while self.host.run_state() == RunState::Running
                && self.host.play_replaying()
                && Instant::now() < slice_end
            {
                self.run_frame(live);
            }
            if !self.host.play_replaying() {
                self.next_frame = Instant::now();
            }
            return;
        }
        let now = Instant::now();
        if now.saturating_duration_since(self.next_frame) > MAX_PACING_LAG {
            self.next_frame = now;
        }
        let mut ran = 0;
        while self.host.run_state() == RunState::Running
            && Instant::now() >= self.next_frame
            && ran < MAX_CATCHUP_FRAMES
        {
            self.run_frame(live);
            let period = guest_frame_dt(Some(self.host.machine().bus().vblank_period()));
            self.next_frame += Duration::from_secs_f32(period);
            ran += 1;
        }
    }

    /// Present the guest display, re-uploading it only when the bus moved.
    fn present(&mut self) {
        let Some(gfx) = self.graphics.as_mut() else {
            return;
        };
        let bus = self.host.machine().bus();
        let cycles = bus.cycles();
        if self.presented_cycles != Some(cycles) {
            gfx.upload_display(&bus.gpu, self.deinterlace);
            self.presented_cycles = Some(cycles);
        }
        let texture = gfx.display_texture_id();
        let uv = cpu_display_uv(bus.gpu.display_area());
        let mut present_size_px = (0, 0);
        gfx.render(|ctx| {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE.fill(egui::Color32::BLACK))
                .show(ctx, |ui| {
                    crate::ui::framebuffer::draw(ui, texture, uv, &mut present_size_px);
                });
        });
    }

    /// Save the session's tape and leave the event loop.
    fn close(&mut self, event_loop: &ActiveEventLoop) {
        match self.host.save_play_session() {
            Ok(true) => eprintln!("[play-session] saved the session tape"),
            Ok(false) => eprintln!(
                "[play-session] tape left as it was (the replay had not handed over, or the tape was unreadable or a non-Play tape)"
            ),
            Err(error) => {
                eprintln!("[play-session] saving the session tape failed: {error}");
                self.exit = Err(error);
            }
        }
        event_loop.exit();
    }
}

impl ApplicationHandler for PlayWindow {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.graphics.is_some() {
            return;
        }
        crate::app_icon::set_application_icon();
        self.title = self.current_title();
        let mut attrs = Window::default_attributes()
            .with_title(self.title.clone())
            .with_inner_size(winit::dpi::PhysicalSize::new(
                WINDOWED_WIDTH,
                WINDOWED_HEIGHT,
            ));
        if let Some(icon) = crate::app_icon::load_window_icon() {
            attrs = attrs.with_window_icon(Some(icon));
        }
        if self.fullscreen {
            attrs = attrs.with_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
        }
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        self.graphics = Some(pollster::block_on(Graphics::new(window)));
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(gfx) = self.graphics.as_mut() else {
            return;
        };
        let _ = gfx.egui_winit.on_window_event(&gfx.window, &event);
        match event {
            WindowEvent::CloseRequested => self.close(event_loop),
            WindowEvent::Resized(size) => {
                gfx.resize(size);
                gfx.window.request_redraw();
            }
            // Releases of keys held across a focus change may never arrive.
            WindowEvent::Focused(false) => self.host_input.clear(),
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key,
                        state,
                        repeat,
                        ..
                    },
                ..
            } if !repeat => {
                self.host_input
                    .apply_key_event(&physical_key, state, &self.bindings);
                if state == ElementState::Pressed {
                    self.input.note_keyboard_press(
                        key_to_pad_button(&physical_key, &self.bindings).unwrap_or(0),
                        key_is_analog_button(&physical_key, &self.bindings),
                    );
                }
            }
            WindowEvent::RedrawRequested => {
                self.last_redraw = Instant::now();
                if let Some(jobs) = self.jobs.as_ref() {
                    if !transport::drain_jobs(&mut self.host, jobs) {
                        eprintln!("[play-session] emucap transport ended; playing on without it");
                        self.jobs = None;
                    }
                    self.report_replay();
                }
                let live = self.live_port1();
                self.advance(live);
                self.update_title();
                self.present();
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(gfx) = self.graphics.as_ref() else {
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        };
        let now = Instant::now();
        let due = if self.host.run_state() != RunState::Running {
            self.last_redraw + FROZEN_TICK
        } else if self.host.play_replaying() {
            now
        } else {
            self.next_frame
        };
        if due <= now {
            gfx.window.request_redraw();
            event_loop.set_control_flow(ControlFlow::WaitUntil(now + FROZEN_TICK));
        } else {
            event_loop.set_control_flow(ControlFlow::WaitUntil(due));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(report: ReplayReport) -> ReplayResult {
        ReplayResult {
            report,
            image_changed: false,
        }
    }

    #[test]
    fn title_shows_the_replay_outcome_and_agent_freeze() {
        assert_eq!(
            window_title(true, None, None, false),
            "PSoXide Play — replaying…"
        );
        assert_eq!(
            window_title(false, Some(&result(ReplayReport::SameState)), None, false),
            "PSoXide Play — same state"
        );
        assert_eq!(
            window_title(
                false,
                Some(&result(ReplayReport::Diverged {
                    expected: 0x1,
                    actual: 0xab
                })),
                None,
                false
            ),
            "PSoXide Play — DIVERGED (expected 0000000000000001, got 00000000000000ab)"
        );
        assert_eq!(
            window_title(false, Some(&result(ReplayReport::NoTape)), None, false),
            "PSoXide Play — new recording"
        );
        let refused = result(ReplayReport::TapeUnreadable("x.tape: non-Play".into()));
        assert_eq!(
            window_title(false, Some(&refused), Some("x.tape: non-Play"), false),
            "PSoXide Play — tape not saved: x.tape: non-Play"
        );
        assert_eq!(
            window_title(true, None, None, true),
            "PSoXide Play — replaying… — frozen by agent"
        );
        let changed = ReplayResult {
            report: ReplayReport::SameState,
            image_changed: true,
        };
        assert_eq!(
            window_title(false, Some(&changed), None, false),
            "PSoXide Play — same state, game image changed"
        );
    }
}
