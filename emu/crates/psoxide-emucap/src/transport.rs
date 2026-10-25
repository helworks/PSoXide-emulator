//! Producer transport: connect to the Control MCP, authenticate, and shuttle
//! NDJSON between the socket and the emulator thread.
//!
//! ## Threading
//! Same split as the GUI's `mcp.rs`: the emulator (`Host`) never leaves its
//! thread. This transport thread owns the socket; each request becomes a
//! [`Job`] sent over a channel, executed by the emulator loop between frames
//! ([`serve_jobs`]), and its terminal [`Response`] comes back over a reply
//! channel. While a request executes the transport writes a `working`
//! keepalive every second (emucap `TIME.PROGRESS.1`); if that write fails the
//! peer is gone and the job's cancel flag is raised, so the advance stops at
//! the next batch instead of running to its host deadline (`TIME.CANCEL.1`).
//!
//! Requests are served strictly one at a time, as emucap's direct link sends
//! them. The connection is re-established with backoff whenever it drops; the
//! emulator keeps its state across reconnects.

use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::host::{Host, RunState};
use crate::wire::{read_frame, Request, Response};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const AUTH_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const READ_POLL: Duration = Duration::from_millis(250);
const WORKING_INTERVAL: Duration = Duration::from_secs(1);
const MIN_RETRY: Duration = Duration::from_millis(50);
const MAX_RETRY: Duration = Duration::from_secs(2);

/// PS1 master clock, for real-time pacing of free-running frames.
const CPU_HZ: f64 = 33_868_800.0;

/// One request handed to the emulator thread.
pub struct Job {
    /// The request.
    pub request: Request,
    /// Raised when the requesting connection is gone.
    pub cancel: Arc<AtomicBool>,
    /// Where the terminal response goes.
    pub reply: Sender<Response>,
}

/// Managed-launch credentials and endpoint.
#[derive(Debug, Clone)]
pub struct ProducerConfig {
    /// The Control MCP's direct listener port (`EMUCAP_PORT`).
    pub port: u16,
    /// `EMUCAP_SESSION_TOKEN`.
    pub front_key: String,
    /// `EMUCAP_LAUNCH_ID`.
    pub launch_id: String,
}

/// Emulator-thread side: execute jobs between frames until every sender is
/// gone. While running, frames advance in real time (or flat out when
/// `unthrottled`), and pending jobs are drained before every frame.
pub fn serve_jobs(host: &mut Host, jobs: &Receiver<Job>, unthrottled: bool) {
    let mut next_frame = Instant::now();
    loop {
        if host.run_state() == RunState::Running {
            if !drain_jobs(host, jobs) {
                return;
            }
            if host.run_state() != RunState::Running {
                continue;
            }
            if !unthrottled {
                let now = Instant::now();
                if now < next_frame {
                    match jobs.recv_timeout(next_frame - now) {
                        Ok(job) => {
                            execute(host, job);
                            continue;
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                } else if now - next_frame > Duration::from_millis(250) {
                    // Fell far behind (a long request ran): re-anchor instead of
                    // racing to catch up.
                    next_frame = now;
                }
            }
            host.run_free_frame();
            let period = host.machine().bus().vblank_period().max(1) as f64 / CPU_HZ;
            next_frame += Duration::from_secs_f64(period);
        } else {
            match jobs.recv() {
                Ok(job) => execute(host, job),
                Err(_) => return,
            }
            next_frame = Instant::now();
        }
    }
}

/// Execute every job already queued, without waiting for more: the per-frame
/// drain of a loop that owns its own pacing (the frontend's play-session
/// window). Returns false once every sender is gone, after running what they
/// queued.
pub fn drain_jobs(host: &mut Host, jobs: &Receiver<Job>) -> bool {
    loop {
        match jobs.try_recv() {
            Ok(job) => execute(host, job),
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Disconnected) => return false,
        }
    }
}

fn execute(host: &mut Host, job: Job) {
    let response = host.handle(&job.request, Some(&job.cancel));
    let _ = job.reply.send(response);
}

/// Transport-thread side for a managed launch: connect, authenticate, serve,
/// and reconnect forever (until the job channel closes).
pub fn run_producer(config: &ProducerConfig, jobs: &Sender<Job>) {
    let endpoint = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), config.port);
    let mut delay = MIN_RETRY;
    loop {
        let mut stream = match TcpStream::connect_timeout(&endpoint, CONNECT_TIMEOUT) {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("[emucap] control MCP unavailable at {endpoint} ({error}); retrying in {delay:?}");
                std::thread::sleep(delay);
                delay = (delay * 2).min(MAX_RETRY);
                continue;
            }
        };
        let authenticated = stream
            .set_read_timeout(Some(AUTH_TIMEOUT))
            .and_then(|()| stream.set_write_timeout(Some(AUTH_TIMEOUT)))
            .map_err(|error| error.to_string())
            .and_then(|()| {
                crate::auth::authenticate(
                    &mut stream,
                    &config.front_key,
                    &config.launch_id,
                    config.port,
                )
            });
        if let Err(error) = authenticated {
            eprintln!("[emucap] authentication failed ({error}); retrying in {delay:?}");
            std::thread::sleep(delay);
            delay = (delay * 2).min(MAX_RETRY);
            continue;
        }
        delay = MIN_RETRY;
        eprintln!("[emucap] connected to control MCP on port {}", config.port);
        match serve_stream(stream, jobs) {
            Ok(SessionEnd::Disconnected) => {
                eprintln!("[emucap] control MCP disconnected; reconnecting")
            }
            Ok(SessionEnd::EmulatorGone) => return,
            Err(error) => eprintln!("[emucap] session failed ({error}); reconnecting"),
        }
        std::thread::sleep(delay);
    }
}

/// How a session ended.
#[derive(Debug, PartialEq, Eq)]
pub enum SessionEnd {
    /// The peer closed the connection.
    Disconnected,
    /// The emulator loop is gone; the process is shutting down.
    EmulatorGone,
}

/// Serve one authenticated connection until EOF.
pub fn serve_stream(stream: TcpStream, jobs: &Sender<Job>) -> std::io::Result<SessionEnd> {
    stream.set_nodelay(true).ok();
    stream.set_read_timeout(Some(READ_POLL))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    serve_lines(&mut reader, &mut writer, jobs, true)
}

/// Serve NDJSON from `reader` to `writer` (a socket, or stdin/stdout for
/// `--stdio`). `poll_timeouts` treats read timeouts as idle polls.
pub fn serve_lines<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    jobs: &Sender<Job>,
    poll_timeouts: bool,
) -> std::io::Result<SessionEnd> {
    let mut pending = Vec::new();
    loop {
        let line = match read_frame(reader, &mut pending) {
            Ok(Some(line)) => line,
            Ok(None) => return Ok(SessionEnd::Disconnected),
            Err(error)
                if poll_timeouts
                    && matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if is_control_session_event(line) {
            // Broker attach/detach notices carry no request id; the direct
            // producer has no attachment state to update.
            continue;
        }
        let request = match Request::parse(line) {
            Ok(request) => request,
            Err((id, message)) => {
                write_line(writer, &Response::err(id, "protocol_error", message))?;
                continue;
            }
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let (reply_tx, reply_rx) = mpsc::channel();
        let job = Job {
            request: request.clone(),
            cancel: Arc::clone(&cancel),
            reply: reply_tx,
        };
        if jobs.send(job).is_err() {
            write_line(
                writer,
                &Response::err(request.id, "emulator_error", "emulator loop has exited"),
            )?;
            return Ok(SessionEnd::EmulatorGone);
        }
        let mut write_error = None;
        let response = loop {
            match reply_rx.recv_timeout(WORKING_INTERVAL) {
                Ok(response) => break response,
                Err(RecvTimeoutError::Timeout) => {
                    if write_error.is_none() {
                        if let Err(error) = write_line(writer, &Response::working(request.id)) {
                            cancel.store(true, Ordering::Release);
                            write_error = Some(error);
                        }
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    break Response::err(
                        request.id,
                        "emulator_error",
                        "emulator loop dropped the request",
                    );
                }
            }
        };
        if let Some(error) = write_error {
            return Err(error);
        }
        write_line(writer, &response)?;
    }
}

fn is_control_session_event(line: &str) -> bool {
    serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|value| {
            value
                .as_object()
                .map(|object| object.len() == 1 && object.contains_key("_control_session"))
        })
        .unwrap_or(false)
}

fn write_line<W: Write>(writer: &mut W, response: &Response) -> std::io::Result<()> {
    writer.write_all(&response.to_line())?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::HostIdentity;
    use crate::machine::{BootConfig, Content, Machine};
    use serde_json::json;

    const KEY: &str =
        "front-direct-89abcdef0123456789abcdef0123456789abcdef0123456789abcdef01234567";
    const LAUNCH: &str = "launch-01hzx7k3m9q2w4e6r8t0y1v3j5";

    /// `j entry; nop` at 0x80010000.
    fn spin_exe() -> Vec<u8> {
        let mut exe = vec![0u8; 0x800];
        exe[..8].copy_from_slice(b"PS-X EXE");
        exe[0x10..0x14].copy_from_slice(&0x8001_0000u32.to_le_bytes());
        exe[0x18..0x1c].copy_from_slice(&0x8001_0000u32.to_le_bytes());
        exe[0x1c..0x20].copy_from_slice(&8u32.to_le_bytes());
        exe.extend_from_slice(&(0x0800_4000u32).to_le_bytes());
        exe.extend_from_slice(&0u32.to_le_bytes());
        exe
    }

    fn read_response(reader: &mut impl BufRead) -> Value {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn producer_authenticates_and_serves_hello_like_a_native_host() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        // Producer: connect, authenticate, serve; emulator loop on its own thread.
        let (jobs_tx, jobs_rx) = mpsc::channel::<Job>();
        let emulator = std::thread::spawn(move || {
            let machine = Machine::boot(BootConfig::new(Content::ExeBytes {
                label: "spin.exe".into(),
                bytes: spin_exe(),
            }))
            .unwrap();
            let mut host = Host::new(
                machine,
                HostIdentity {
                    launch_id: Some(LAUNCH.into()),
                    start_frozen: true,
                    ..Default::default()
                },
            );
            serve_jobs(&mut host, &jobs_rx, true);
        });
        let producer = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            stream.set_read_timeout(Some(AUTH_TIMEOUT)).unwrap();
            crate::auth::authenticate(&mut stream, KEY, LAUNCH, port).unwrap();
            serve_stream(stream, &jobs_tx).unwrap()
        });

        // Control MCP side: authenticate the producer, then speak NDJSON.
        let (mut socket, _) = listener.accept().unwrap();
        let launch = crate::auth::accept_for_test(&mut socket, KEY, port, [7; 16], false).unwrap();
        assert_eq!(launch, LAUNCH);
        socket
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        let mut send = |line: String| {
            socket.write_all(line.as_bytes()).unwrap();
            socket.write_all(b"\n").unwrap();
        };

        send(json!({"v": 1, "id": 0, "method": "hello", "params": {}}).to_string());
        let hello = read_response(&mut reader);
        assert_eq!(hello["id"], 0);
        assert_eq!(hello["ok"], true);
        assert_eq!(hello["result"]["protocol_version"], 1);
        assert_eq!(hello["result"]["launch_id"], LAUNCH);

        // Broker session notices are not requests and get no reply; a
        // malformed request gets an explicit protocol error.
        send(r#"{"_control_session":{"kind":"attach","runtime":"r","attachment":{}}}"#.into());
        send(r#"{"v":1,"id":5,"method":"status"}"#.into());
        let malformed = read_response(&mut reader);
        assert_eq!(malformed["id"], 5);
        assert_eq!(malformed["error"]["kind"], "protocol_error");

        send(json!({"v": 1, "id": 6, "method": "step", "params": {"frames": 3}}).to_string());
        let step = read_response(&mut reader);
        assert_eq!(step["id"], 6);
        assert_eq!(step["result"]["frame"], 3);

        drop(reader);
        socket.shutdown(std::net::Shutdown::Both).unwrap();
        assert_eq!(producer.join().unwrap(), SessionEnd::Disconnected);
        // The producer dropped its job sender; the emulator loop ends.
        emulator.join().unwrap();
    }

    #[test]
    fn drain_jobs_runs_every_queued_job_and_reports_when_the_senders_are_gone() {
        let machine = Machine::boot(BootConfig::new(Content::ExeBytes {
            label: "spin.exe".into(),
            bytes: spin_exe(),
        }))
        .unwrap();
        let mut host = Host::new(
            machine,
            HostIdentity {
                start_frozen: true,
                ..Default::default()
            },
        );
        let (jobs_tx, jobs_rx) = mpsc::channel::<Job>();
        let (reply_tx, reply_rx) = mpsc::channel();
        for (id, frames) in [(1, 2), (2, 3)] {
            let line = json!({"v": 1, "id": id, "method": "step", "params": {"frames": frames}});
            jobs_tx
                .send(Job {
                    request: Request::parse(&line.to_string()).unwrap(),
                    cancel: Arc::new(AtomicBool::new(false)),
                    reply: reply_tx.clone(),
                })
                .unwrap();
        }

        assert!(drain_jobs(&mut host, &jobs_rx), "a sender is still alive");
        let replies: Vec<Response> = reply_rx.try_iter().collect();
        assert_eq!(replies.len(), 2, "both queued jobs ran in one drain");
        assert_eq!(host.machine().frame(), 5, "in order, to completion");
        assert!(
            drain_jobs(&mut host, &jobs_rx),
            "an empty queue is not the end"
        );

        drop(jobs_tx);
        assert!(!drain_jobs(&mut host, &jobs_rx), "every sender is gone");
    }

    #[test]
    fn slow_requests_are_kept_alive_and_cancelled_when_the_peer_leaves() {
        let (jobs_tx, jobs_rx) = mpsc::channel::<Job>();
        let worker = std::thread::spawn(move || {
            let job = jobs_rx.recv().unwrap();
            std::thread::sleep(Duration::from_millis(2_300));
            let cancelled = job.cancel.load(Ordering::Acquire);
            let _ = job
                .reply
                .send(Response::ok(job.request.id, json!({"status": "completed"})));
            cancelled
        });
        let input = format!(
            "{}\n",
            json!({"v": 1, "id": 9, "method": "step", "params": {}})
        );
        let mut reader = std::io::Cursor::new(input.into_bytes());
        let mut output = Vec::new();
        assert_eq!(
            serve_lines(&mut reader, &mut output, &jobs_tx, false).unwrap(),
            SessionEnd::Disconnected
        );
        assert!(!worker.join().unwrap(), "a live peer is never cancelled");
        let text = String::from_utf8(output).unwrap();
        let frames: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert!(
            frames.len() >= 3,
            "two keepalives then the terminal: {text}"
        );
        assert!(frames[..frames.len() - 1]
            .iter()
            .all(|f| f["id"] == 9 && f["result"]["status"] == "working"));
        assert_eq!(frames.last().unwrap()["result"]["status"], "completed");

        // A writer that fails behaves like a vanished peer: cancel is raised.
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (jobs_tx, jobs_rx) = mpsc::channel::<Job>();
        let worker = std::thread::spawn(move || {
            let job = jobs_rx.recv().unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !job.cancel.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = job.reply.send(Response::ok(
                job.request.id,
                json!({"status": "interrupted"}),
            ));
            job.cancel.load(Ordering::Acquire)
        });
        let input = format!(
            "{}\n",
            json!({"v": 1, "id": 10, "method": "step", "params": {}})
        );
        let mut reader = std::io::Cursor::new(input.into_bytes());
        assert!(serve_lines(&mut reader, &mut Broken, &jobs_tx, false).is_err());
        assert!(worker.join().unwrap(), "the job saw its cancel flag");
    }
}
