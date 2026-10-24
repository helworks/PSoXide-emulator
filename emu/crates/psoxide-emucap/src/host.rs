//! emucap method dispatch over a [`Machine`].
//!
//! [`Host::handle`] turns one wire [`Request`] into one terminal [`Response`].
//! It runs on the emulator thread between frames, so it owns the machine for
//! the whole request: a read can never observe a half-executed frame, and an
//! advance is never raced by free-running emulation.
//!
//! Only methods with an emucap feature contract (`contracts/catalog.json`) are
//! listed in `methods`; PSoXide-specific operations are prefixed `psoxide_` and
//! listed separately under `extension_methods` until emucap adopts them.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{json, Map, Value};
use sha2::Digest as _;

use crate::machine::{
    button_bit, button_names, AdvanceControl, Content, Machine, PortInput, Stop, Unit,
};
use crate::memory::{self, MemoryType};
use crate::wire::{Request, Response, PROTOCOL_VERSION};

/// emucap's synchronous advance cap (`live::temporal::MAX_SYNC_ADVANCE_COUNT`).
pub const MAX_SYNC_ADVANCE_COUNT: u64 = 5_000;

/// Host budget for one synchronous request, under emucap's 250 s
/// `MAX_SYNC_OPERATION_MS` so the terminal response always beats the transport.
pub const ADVANCE_BUDGET: Duration = Duration::from_secs(245);

/// Largest single `read_memory` / `write_memory` transfer.
pub const MAX_MEMORY_TRANSFER: u64 = 0x1_0000;

/// emucap's PSX system id (`launch/system.rs::normalize_system`).
pub const SYSTEM: &str = "psx";

/// Adapter id reported in `hello`/`status`.
pub const ADAPTER: &str = "psoxide";

const CPU_ID: &str = "r3000a";

/// Contract methods this host implements.
pub const METHODS: &[&str] = &[
    "hello",
    "status",
    "get_rom_info",
    "get_state",
    "read_memory",
    "write_memory",
    "read_memory_batch",
    "pause",
    "resume",
    "step",
    "step_instructions",
    "step_input_polls",
    "set_input",
    "press_buttons",
    "screenshot",
    "save_state",
    "load_state",
    "reset",
    "poll_events",
    "set_breakpoint",
    "clear_breakpoint",
    "list_breakpoints",
    "clear_all_breakpoints",
];

/// PSoXide-specific methods, callable on the wire but not advertised in
/// `methods` (they have no emucap feature contract yet).
pub const EXTENSION_METHODS: &[&str] = &[
    "psoxide_hashes",
    "psoxide_input_tape",
    "psoxide_run_to_poll",
];

/// emucap contract exceptions this host activates (`contracts/exceptions.json`,
/// scope `adapter: "psoxide"`, `systems: ["psx"]`). Each names a documented,
/// explicitly-failing narrowing of a catalog expectation.
pub const ACTIVE_EXCEPTIONS: &[&str] = &[
    "psoxide.memory-write.read-only-spaces",
    "psoxide.memory-read.cpu-io-absent",
    "psoxide.breakpoint.exec-pausing-only",
    "psoxide.state-load.external-media",
    "psoxide.screenshot.display-required",
];

/// Per-process identity reported in `hello`.
#[derive(Debug, Clone, Default)]
pub struct HostIdentity {
    /// `EMUCAP_LAUNCH_ID`, when launched by emucap.
    pub launch_id: Option<String>,
    /// `EMUCAP_NAME`.
    pub name: Option<String>,
    /// `EMUCAP_CONTENT`, falling back to the booted path.
    pub content: Option<String>,
    /// Whether the launch asked for a frozen start (`EMUCAP_START_FROZEN=1`).
    pub start_frozen: bool,
}

/// What a request did to the run state, for the emulator loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    /// Stopped between requests.
    Frozen,
    /// Free-running: the loop steps frames between requests.
    Running,
}

/// The dispatcher. Owns the machine and the bits of host state emucap observes.
pub struct Host {
    machine: Machine,
    identity: HostIdentity,
    run_state: RunState,
    rom_info: Option<Value>,
}

type HostResult = Result<Value, (&'static str, String)>;

fn bad_params(message: impl Into<String>) -> (&'static str, String) {
    ("bad_params", message.into())
}

fn bad_state(message: impl Into<String>) -> (&'static str, String) {
    ("bad_state", message.into())
}

impl Host {
    /// Wrap a booted machine. It starts frozen when the launch asked for it,
    /// running otherwise.
    pub fn new(machine: Machine, identity: HostIdentity) -> Self {
        let run_state = if identity.start_frozen {
            RunState::Frozen
        } else {
            RunState::Running
        };
        Self {
            machine,
            identity,
            run_state,
            rom_info: None,
        }
    }

    /// The machine, for the emulator loop.
    pub fn machine(&self) -> &Machine {
        &self.machine
    }

    /// The machine, mutably, for the emulator loop's per-frame work (a Play
    /// session's replay check and recording).
    pub fn machine_mut(&mut self) -> &mut Machine {
        &mut self.machine
    }

    /// Current run state.
    pub fn run_state(&self) -> RunState {
        self.run_state
    }

    /// Advance one frame of free-running emulation (the emulator loop's job
    /// while running). Faults or tape stops freeze the machine.
    pub fn run_free_frame(&mut self) {
        if self.run_state != RunState::Running {
            return;
        }
        let control = AdvanceControl {
            deadline: Instant::now() + ADVANCE_BUDGET,
            cancel: None,
        };
        let advance = self.machine.advance(Unit::Frames, 1, &control);
        if advance.stop != Stop::Completed {
            self.run_state = RunState::Frozen;
        }
    }

    /// Execute one request to its terminal response. `cancel` is raised by the
    /// transport if the requesting connection drops mid-request.
    pub fn handle(&mut self, request: &Request, cancel: Option<&AtomicBool>) -> Response {
        let params = &request.params;
        if let Some(key) = params
            .keys()
            .find(|key| key.starts_with("_control") || key.as_str() == "_temporal_owner")
        {
            return Response::err(
                request.id,
                "unsupported",
                format!("{key}: owned temporal control is not implemented by the PSoXide host"),
            );
        }
        let result = match request.method.as_str() {
            "hello" => Ok(self.hello()),
            "status" => Ok(self.status()),
            "get_rom_info" => self.get_rom_info(),
            "get_state" => self.get_state(params),
            "read_memory" => self.read_memory(params),
            "write_memory" => self.write_memory(params),
            "read_memory_batch" => self.read_memory_batch(params),
            "pause" => self.pause(params),
            "resume" => self.resume(params),
            "step" => self.step(params, Unit::Frames, cancel),
            "step_instructions" => self.step(params, Unit::Instructions, cancel),
            "step_input_polls" => self.step(params, Unit::InputPolls, cancel),
            "set_input" => self.set_input(params),
            "press_buttons" => self.press_buttons(params, cancel),
            "screenshot" => self.screenshot(params),
            "save_state" => self.save_state(params),
            "load_state" => self.load_state(params),
            "reset" => self.reset(params),
            "poll_events" => Ok(self.poll_events()),
            "set_breakpoint" => self.set_breakpoint(params),
            "clear_breakpoint" => self.clear_breakpoint(params),
            "list_breakpoints" => Ok(self.list_breakpoints()),
            "clear_all_breakpoints" => {
                Ok(json!({"status": "completed", "cleared": self.machine.clear_breakpoints()}))
            }
            "psoxide_hashes" => Ok(self.hashes()),
            "psoxide_input_tape" => self.input_tape(params),
            "psoxide_run_to_poll" => self.run_to_poll(params, cancel),
            other => Err((
                "unsupported",
                format!("unsupported PSoXide method: {other}"),
            )),
        };
        match result {
            Ok(value) => Response::ok(request.id, value),
            Err((kind, message)) => Response::err(request.id, kind, message),
        }
    }

    fn state_name(&self) -> &'static str {
        match self.run_state {
            RunState::Frozen => "frozen",
            RunState::Running => "running",
        }
    }

    // --- discovery -----------------------------------------------------------

    fn hello(&self) -> Value {
        let mut value = json!({
            "protocol_version": PROTOCOL_VERSION,
            "system": SYSTEM,
            "adapter": ADAPTER,
            "backend": "psoxide-emulator-core",
            "build": env!("CARGO_PKG_VERSION"),
            "debugger": true,
            "methods": METHODS,
            "extension_methods": EXTENSION_METHODS,
            "memory_types": memory::type_names(),
            "memory_regions": memory::regions(),
            "state_groups": ["cpu", "cop0"],
            "cpu_targets": [{
                "id": CPU_ID,
                "aliases": ["cpu", "main", "maincpu", "mips"],
                "default": true,
            }],
            "breakpoint_kinds": breakpoint_kinds(),
            "input_buttons": button_names(0xffff),
            "input_axes": input_axes(),
            "input_poll_capability": {
                "port": 0,
                "domain": "port1_completed_controller_poll",
                "counter": "generation_monotonic",
                "guest_counter": "guest_pad_polls (Bus::port1_completed_polls; restarts at 0 on load_state/reset, the PXITAPE2 clock)",
                "boundary": "instruction_completing_the_poll",
                "step_method": "step_input_polls",
            },
            "host_features": ["controlled_start"],
            "execution_limits": execution_limits(),
            "contracts": {
                "catalog": "emucap-feature-contracts/v3",
                "active_exceptions": ACTIVE_EXCEPTIONS,
            },
            "memory_batch_capability": memory::batch_capability(),
            "capability_notes": capability_notes(),
        });
        let object = value.as_object_mut().expect("hello is an object");
        if let Some(name) = &self.identity.name {
            object.insert("name".into(), json!(name));
        }
        if let Some(content) = self.content_label() {
            object.insert("content".into(), json!(content));
        }
        if let Some(launch_id) = &self.identity.launch_id {
            object.insert("launch_id".into(), json!(launch_id));
        }
        value
    }

    fn content_label(&self) -> Option<String> {
        self.identity
            .content
            .clone()
            .or_else(|| match &self.machine.config().content {
                Content::File(path) => Some(path.display().to_string()),
                Content::ExeBytes { label, .. } => Some(label.clone()),
            })
    }

    fn status(&self) -> Value {
        let machine = &self.machine;
        let (_, width, height, _) = machine.display_hash();
        json!({
            "connected": true,
            "system": SYSTEM,
            "adapter": ADAPTER,
            "debugger": true,
            "state": self.state_name(),
            "boundary": machine.boundary().as_str(),
            "frame": machine.frame(),
            "input_polls": machine.input_polls(),
            "guest_pad_polls": machine.guest_pad_polls(),
            "cpu_tick": machine.cpu().tick(),
            "bus_cycles": machine.bus().cycles(),
            "pc": machine.cpu().pc(),
            "halted": machine.halted(),
            "display": { "width": width, "height": height },
            "methods": METHODS,
            "memory_types": memory::type_names(),
            "input_override": input_override_json(machine),
            "input_tape": tape_json(machine),
            "execution_limits": execution_limits(),
            "launch_start": {
                "requested_frozen": self.identity.start_frozen,
                "controlled": self.identity.start_frozen,
                "boundary": if self.identity.start_frozen { json!("pre_first_instruction") } else { Value::Null },
            },
        })
    }

    fn get_rom_info(&mut self) -> HostResult {
        if let Some(info) = &self.rom_info {
            return Ok(info.clone());
        }
        let info = match &self.machine.config().content {
            Content::File(path) => {
                let (sha1, sha256, size) = hash_file(path).map_err(|e| ("io_error", e))?;
                json!({
                    "system": SYSTEM,
                    "adapter": ADAPTER,
                    "name": path.file_name().and_then(|n| n.to_str()).unwrap_or(""),
                    "path": std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()).display().to_string(),
                    "media_type": path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase(),
                    "size": size,
                    "sha1": sha1,
                    "sha256": sha256,
                    "identity_scope": "content_file_bytes",
                })
            }
            Content::ExeBytes { label, bytes } => json!({
                "system": SYSTEM,
                "adapter": ADAPTER,
                "name": label,
                "media_type": "exe",
                "size": bytes.len(),
                "sha1": hex(&sha1::Sha1::digest(bytes)),
                "sha256": hex(&sha2::Sha256::digest(bytes)),
                "identity_scope": "content_file_bytes",
            }),
        };
        self.rom_info = Some(info.clone());
        Ok(info)
    }

    // --- state + memory ------------------------------------------------------

    fn get_state(&self, params: &Map<String, Value>) -> HostResult {
        require_cpu(params)?;
        let groups = match params.get("groups") {
            None => vec!["cpu".to_string(), "cop0".to_string()],
            Some(Value::Array(values)) => values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_ascii_lowercase)
                        .filter(|name| name == "cpu" || name == "cop0")
                        .ok_or_else(|| {
                            bad_params(format!("unknown state group {value}; available: cpu, cop0"))
                        })
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => return Err(bad_params("groups must be an array of names")),
        };
        let cpu = self.machine.cpu();
        let mut state = Map::new();
        if groups.iter().any(|g| g == "cpu") {
            state.insert("cpu.pc".into(), json!(cpu.pc()));
            state.insert("cpu.hi".into(), json!(cpu.hi()));
            state.insert("cpu.lo".into(), json!(cpu.lo()));
            for index in 0..32u8 {
                state.insert(format!("cpu.r{index}"), json!(cpu.gpr(index)));
            }
        }
        if groups.iter().any(|g| g == "cop0") {
            let cop0 = cpu.cop0();
            for (index, name) in [
                (3, "bpc"),
                (5, "bda"),
                (6, "jumpdest"),
                (7, "dcic"),
                (8, "badvaddr"),
                (9, "bdam"),
                (11, "bpcm"),
                (12, "sr"),
                (13, "cause"),
                (14, "epc"),
                (15, "prid"),
            ] {
                state.insert(format!("cop0.{name}"), json!(cop0[index]));
            }
        }
        Ok(json!({
            "cpu": CPU_ID,
            "state": state,
            "groups_applied": groups,
            "frame": self.machine.frame(),
            "cpu_tick": cpu.tick(),
            "bus_cycles": self.machine.bus().cycles(),
            "execution_state": self.state_name(),
            "boundary": self.machine.boundary().as_str(),
        }))
    }

    fn read_memory(&self, params: &Map<String, Value>) -> HostResult {
        let memory_type = memory_type_param(params)?;
        let address = required_u64(params, "address")?;
        let length = required_u64(params, "length")?;
        if length == 0 || length > MAX_MEMORY_TRANSFER {
            return Err(bad_params(format!(
                "length must be 1..={MAX_MEMORY_TRANSFER:#x}"
            )));
        }
        let bytes =
            memory::read(&self.machine, memory_type, address, length).map_err(bad_params)?;
        Ok(json!({
            "memory_type": memory_type.name(),
            "address": address,
            "length": length,
            "hex": hex(&bytes),
            "frame": self.machine.frame(),
            "execution_state": self.state_name(),
        }))
    }

    fn write_memory(&mut self, params: &Map<String, Value>) -> HostResult {
        let memory_type = memory_type_param(params)?;
        let address = required_u64(params, "address")?;
        let raw = params
            .get("hex")
            .and_then(Value::as_str)
            .ok_or_else(|| bad_params("missing required param: hex"))?;
        let bytes = parse_hex(raw).map_err(bad_params)?;
        if bytes.is_empty() || bytes.len() as u64 > MAX_MEMORY_TRANSFER {
            return Err(bad_params(format!(
                "hex must hold 1..={MAX_MEMORY_TRANSFER:#x} bytes"
            )));
        }
        memory::write(&mut self.machine, memory_type, address, &bytes)
            .map_err(|message| ("unsupported", message))?;
        Ok(json!({
            "memory_type": memory_type.name(),
            "address": address,
            "written": bytes.len(),
            "execution_state": self.state_name(),
        }))
    }

    fn read_memory_batch(&self, params: &Map<String, Value>) -> HostResult {
        if self.run_state != RunState::Frozen {
            return Err(bad_state(
                "read_memory_batch requires a frozen machine; call pause first",
            ));
        }
        let ranges = params
            .get("ranges")
            .and_then(Value::as_array)
            .ok_or_else(|| bad_params("ranges must be an array"))?;
        let mut parsed = Vec::with_capacity(ranges.len());
        for (index, range) in ranges.iter().enumerate() {
            let object = range
                .as_object()
                .ok_or_else(|| bad_params(format!("range {index} must be an object")))?;
            parsed.push((
                memory_type_param(object)?,
                required_u64(object, "address")?,
                required_u64(object, "length")?,
            ));
        }
        memory::admit_batch(&parsed).map_err(bad_params)?;
        let mut reads = Vec::with_capacity(parsed.len());
        let mut total = 0u64;
        for (index, &(memory_type, address, length)) in parsed.iter().enumerate() {
            let bytes =
                memory::read(&self.machine, memory_type, address, length).map_err(bad_params)?;
            total += length;
            reads.push(json!({
                "index": index,
                "memory_type": memory_type.name(),
                "address": address,
                "length": length,
                "hex": hex(&bytes),
            }));
        }
        let machine = &self.machine;
        Ok(json!({
            "state": "frozen",
            "consistency": "frozen_boundary",
            "boundary": {
                "runtime_generation": self.identity.launch_id.clone().unwrap_or_else(|| format!("unmanaged-{}", std::process::id())),
                "stop_epoch": machine.stop_epoch().to_string(),
                "memory_mapping_epoch": machine.mapping_epoch().to_string(),
                "kind": machine.boundary().as_str(),
                "clocks": [
                    {"domain": "frame", "value": machine.frame()},
                    {"domain": "cpu_tick", "value": machine.cpu().tick()},
                    {"domain": "bus_cycle", "value": machine.bus().cycles()},
                    {"domain": "input_poll", "value": machine.input_polls()},
                ],
            },
            "reads": reads,
            "total_bytes": total,
        }))
    }

    // --- execution -----------------------------------------------------------

    fn pause(&mut self, params: &Map<String, Value>) -> HostResult {
        require_cpu(params)?;
        // Requests run between frames, so the machine is already parked at the
        // boundary the last advance reached; pausing only stops the free run.
        self.run_state = RunState::Frozen;
        Ok(json!({
            "status": "completed",
            "state": "frozen",
            "boundary": self.machine.boundary().as_str(),
            "frame": self.machine.frame(),
            "pc": self.machine.cpu().pc(),
        }))
    }

    fn resume(&mut self, params: &Map<String, Value>) -> HostResult {
        require_cpu(params)?;
        if let Some(error) = self.machine.halted() {
            return Err(bad_state(format!(
                "CPU halted on an execution error ({error}); reset or load_state first"
            )));
        }
        self.run_state = RunState::Running;
        Ok(json!({"status": "completed", "state": "running", "frame": self.machine.frame()}))
    }

    fn step(
        &mut self,
        params: &Map<String, Value>,
        unit: Unit,
        cancel: Option<&AtomicBool>,
    ) -> HostResult {
        require_cpu(params)?;
        let count = match unit {
            Unit::Frames => optional_u64(params, "frames")?.or(optional_u64(params, "count")?),
            _ => optional_u64(params, "count")?,
        }
        .unwrap_or(1);
        if !(1..=MAX_SYNC_ADVANCE_COUNT).contains(&count) {
            return Err(bad_params(format!(
                "{} count must be in 1..={MAX_SYNC_ADVANCE_COUNT}, got {count}",
                unit.as_str()
            )));
        }
        if let Some(error) = self.machine.halted() {
            return Err(bad_state(format!(
                "CPU halted on an execution error ({error}); reset or load_state first"
            )));
        }
        Ok(self.advance_json(unit, count, cancel))
    }

    /// Run an advance and describe it. The machine is frozen afterwards.
    fn advance_json(&mut self, unit: Unit, count: u64, cancel: Option<&AtomicBool>) -> Value {
        let was_running = self.run_state == RunState::Running;
        self.run_state = RunState::Frozen;
        let frame_before = self.machine.frame();
        let polls_before = self.machine.input_polls();
        let tick_before = self.machine.cpu().tick();
        let control = AdvanceControl {
            deadline: Instant::now() + ADVANCE_BUDGET,
            cancel,
        };
        let advance = self.machine.advance(unit, count, &control);
        let machine = &self.machine;
        let mut value = json!({
            "status": if advance.stop == Stop::Completed { "completed" } else { "interrupted" },
            "unit": unit.as_str(),
            "count": advance.completed,
            "requested": advance.requested,
            "completed": advance.completed,
            "cpu": CPU_ID,
            "state": "frozen",
            "was_running": was_running,
            "boundary": machine.boundary().as_str(),
            "pc": machine.cpu().pc(),
            "frame_before": frame_before,
            "frame": machine.frame(),
            "input_polls_before": polls_before,
            "input_polls_after": machine.input_polls(),
            "cpu_tick_delta": machine.cpu().tick() - tick_before,
        });
        if let Stop::Interrupted(reason) = advance.stop {
            value["reason"] = json!(reason);
            if let Some(event) = machine.last_hit() {
                value["breakpoint_id"] = event["breakpoint_id"].clone();
                value["event"] = event.clone();
            }
            if reason == "execution_error" {
                value["error"] = json!(machine.halted());
            }
        }
        value
    }

    fn set_input(&mut self, params: &Map<String, Value>) -> HostResult {
        let port = port_param(params, &self.machine)?;
        let state = decode_input(params, self.machine.port_input(port))?;
        if port == 0 && self.machine.tape().is_some() {
            return Err((
                "busy",
                "an input tape owns port 0; stop it with psoxide_input_tape first".into(),
            ));
        }
        self.machine.set_port_input(port, state);
        let applied = self.machine.port_input(port);
        Ok(json!({
            "status": "completed",
            "port": port,
            "applied_mask": format!("0x{:04x}", applied.buttons),
            "applied_buttons": button_names(applied.buttons),
            "axes": axes_json(applied.sticks),
            "override_engaged": applied.engaged,
            "pad_mode": pad_mode(&self.machine, port),
        }))
    }

    fn press_buttons(
        &mut self,
        params: &Map<String, Value>,
        cancel: Option<&AtomicBool>,
    ) -> HostResult {
        let port = port_param(params, &self.machine)?;
        let pulse = decode_buttons(params.get("buttons"))?;
        if pulse == 0 {
            return Err(bad_params("press_buttons requires at least one button"));
        }
        let frames = optional_u64(params, "frames")?.unwrap_or(1);
        if !(1..=MAX_SYNC_ADVANCE_COUNT).contains(&frames) {
            return Err(bad_params(format!(
                "frames must be in 1..={MAX_SYNC_ADVANCE_COUNT}"
            )));
        }
        if port == 0 && self.machine.tape().is_some() {
            return Err(("busy", "an input tape owns port 0".into()));
        }
        if let Some(error) = self.machine.halted() {
            return Err(bad_state(format!(
                "CPU halted on an execution error ({error})"
            )));
        }
        let was_running = self.run_state == RunState::Running;
        let persistent = self.machine.port_input(port);
        let pressed = PortInput {
            buttons: persistent.buttons | pulse,
            engaged: true,
            ..persistent
        };
        self.machine.set_port_input(port, Some(pressed));
        let advance = self.advance_json(Unit::Frames, frames, cancel);
        // Restore the persistent override whatever the advance did
        // (INPUT.PULSE.CLEANUP.1).
        self.machine
            .set_port_input(port, persistent.engaged.then_some(persistent));
        let interrupted = advance["status"] == "interrupted";
        if was_running && !interrupted {
            self.run_state = RunState::Running;
        }
        let mut value = json!({
            "status": advance["status"],
            "buttons": button_names(pulse),
            "frames": frames,
            "frames_completed": advance["completed"],
            "frame": advance["frame"],
            "persistent_buttons": button_names(persistent.buttons),
            "transient_override_engaged": false,
            "state": self.state_name(),
        });
        if let Some(reason) = advance.get("reason") {
            value["reason"] = reason.clone();
        }
        Ok(value)
    }

    fn reset(&mut self, params: &Map<String, Value>) -> HostResult {
        require_cpu(params)?;
        self.machine
            .reset()
            .map_err(|error| ("emulator_error", error))?;
        Ok(json!({
            "status": "completed",
            "reset": "completed",
            "kind": "power_on_reboot",
            "reset_seq": self.machine.reset_seq(),
            "state": self.state_name(),
            "boundary": self.machine.boundary().as_str(),
            "pc": self.machine.cpu().pc(),
            "input_override_preserved": true,
        }))
    }

    fn set_breakpoint(&mut self, params: &Map<String, Value>) -> HostResult {
        for key in [
            "condition",
            "value",
            "value_mask",
            "value_len",
            "pc_min",
            "pc_max",
            "snapshot",
        ] {
            if params.get(key).is_some_and(|value| !value.is_null()) {
                return Err(bad_params(format!(
                    "breakpoint option {key} is not supported"
                )));
            }
        }
        if params.get("pause_on_hit").and_then(Value::as_bool) == Some(false) {
            return Err(bad_params(
                "PSoXide breakpoints always pause (pause_on_hit=true)",
            ));
        }
        if params.get("auto_savestate").and_then(Value::as_bool) == Some(true) {
            return Err(bad_params("auto_savestate is not supported"));
        }
        let kind = params.get("kind").and_then(Value::as_str).unwrap_or("exec");
        if kind != "exec" {
            return Err(bad_params(format!(
                "unsupported breakpoint kind {kind}; only exec is available"
            )));
        }
        match params.get("memory_type").and_then(Value::as_str) {
            None | Some("cpu") => {}
            Some(other) => {
                return Err(bad_params(format!(
                    "exec breakpoints use cpu addresses, not {other}"
                )))
            }
        }
        let start = required_u64(params, "start")?;
        let end = optional_u64(params, "end")?.unwrap_or(start);
        let to_u32 = |value: u64| {
            u32::try_from(value).map_err(|_| bad_params("breakpoint address exceeds 32 bits"))
        };
        let id = self
            .machine
            .add_breakpoint(to_u32(start)?, to_u32(end)?)
            .map_err(bad_params)?;
        Ok(json!({
            "status": "completed",
            "id": id,
            "kind": "exec",
            "start": start,
            "end": end,
            "pause_on_hit": true,
            "arm_state": "armed",
        }))
    }

    fn clear_breakpoint(&mut self, params: &Map<String, Value>) -> HostResult {
        let id = required_u64(params, "id")?;
        if !self.machine.remove_breakpoint(id) {
            return Err(bad_params(format!("no breakpoint with id {id}")));
        }
        Ok(json!({"status": "completed", "id": id}))
    }

    fn list_breakpoints(&self) -> Value {
        let breakpoints: Vec<Value> = self
            .machine
            .breakpoints()
            .iter()
            .map(|bp| {
                json!({
                    "id": bp.id,
                    "kind": "exec",
                    "memory_type": "cpu",
                    "start": bp.start,
                    "end": bp.end,
                    "hits": bp.hits,
                    "pause_on_hit": true,
                    "arm_state": "armed",
                })
            })
            .collect();
        json!({"breakpoints": breakpoints})
    }

    fn poll_events(&mut self) -> Value {
        let (events, dropped) = self.machine.drain_events();
        json!({"events": events, "dropped": dropped})
    }

    // --- video + state files -------------------------------------------------

    fn screenshot(&self, params: &Map<String, Value>) -> HostResult {
        let source = params
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or("display");
        let (rgba, width, height) = match source {
            "display" => self.machine.bus().gpu.display_rgba8(),
            "vram" => (vram_rgba(&self.machine), 1024, 512),
            other => {
                return Err(bad_params(format!(
                    "unknown screenshot source {other}; use display or vram"
                )))
            }
        };
        if width == 0 || height == 0 {
            return Err(bad_state("the guest has not configured a display yet"));
        }
        let png = encode_png(&rgba, width, height).map_err(|e| ("emulator_error", e))?;
        let (display_hash, ..) = self.machine.display_hash();
        Ok(json!({
            "png_base64": base64::engine::general_purpose::STANDARD.encode(&png),
            "sha256": hex(&sha2::Sha256::digest(&png)),
            "byte_len": png.len(),
            "width": width,
            "height": height,
            "source": source,
            "frame": self.machine.frame(),
            "state": self.state_name(),
            "freshness": "current",
            "capture_boundary": self.machine.boundary().as_str(),
            "display_fnv1a_64": format!("0x{display_hash:016x}"),
        }))
    }

    fn save_state(&mut self, params: &Map<String, Value>) -> HostResult {
        let path = requested_path(params)?;
        let bytes = self
            .machine
            .save_state_bytes()
            .map_err(|e| ("emulator_error", e))?;
        write_atomically(&path, &bytes).map_err(|e| ("io_error", e))?;
        Ok(json!({
            "status": "completed",
            "path": path.display().to_string(),
            "format": "psoxide-savestate-v1",
            "bytes": bytes.len(),
            "sha256": hex(&sha2::Sha256::digest(&bytes)),
            "frame": self.machine.frame(),
            "input_polls": self.machine.input_polls(),
            "cpu_tick": self.machine.cpu().tick(),
            "boundary": self.machine.boundary().as_str(),
            "state": self.state_name(),
            "excludes": ["bios", "disc"],
        }))
    }

    fn load_state(&mut self, params: &Map<String, Value>) -> HostResult {
        let path = requested_path(params)?;
        let bytes = std::fs::read(&path)
            .map_err(|e| bad_params(format!("read {}: {e}", path.display())))?;
        let frame_before = self.machine.frame();
        let game_id = self
            .machine
            .load_state_bytes(&bytes)
            .map_err(|e| bad_params(format!("{}: {e}", path.display())))?;
        self.run_state = RunState::Frozen;
        Ok(json!({
            "status": "completed",
            "path": path.display().to_string(),
            "format": "psoxide-savestate-v1",
            "sha256": hex(&sha2::Sha256::digest(&bytes)),
            "state_game_id": game_id,
            "state": "frozen",
            "frame_before": frame_before,
            "frame": self.machine.frame(),
            "frame_counter_continuous": true,
            "input_polls": self.machine.input_polls(),
            "guest_pad_polls": self.machine.guest_pad_polls(),
            "cpu_tick": self.machine.cpu().tick(),
            "pc": self.machine.cpu().pc(),
            "media": "current_disc_and_bios_preserved",
        }))
    }

    // --- PSoXide extensions --------------------------------------------------

    fn hashes(&self) -> Value {
        let (display, width, height, byte_len) = self.machine.display_hash();
        json!({
            "vram_fnv1a_64": format!("0x{:016x}", self.machine.vram_hash()),
            "display_fnv1a_64": format!("0x{display:016x}"),
            "display_width": width,
            "display_height": height,
            "display_byte_len": byte_len,
            "frame": self.machine.frame(),
            "input_polls": self.machine.input_polls(),
            "cpu_tick": self.machine.cpu().tick(),
            "bus_cycles": self.machine.bus().cycles(),
            "boundary": self.machine.boundary().as_str(),
            "algorithm": "fnv1a-64 (launch --dump-hash)",
        })
    }

    fn input_tape(&mut self, params: &Map<String, Value>) -> HostResult {
        let action = params
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("status");
        match action {
            "status" => Ok(json!({"input_tape": tape_json(&self.machine)})),
            "stop" => {
                let stopped = self.machine.stop_tape();
                Ok(json!({"status": "completed", "stopped": stopped.is_some()}))
            }
            "start" => {
                let path = requested_path(params)?;
                let tape = emulator_core::input_tape::read_tape_full(&path).map_err(bad_params)?;
                let delay = optional_u64(params, "delay_ticks")?.unwrap_or(0);
                let stop_on_end = params
                    .get("stop_on_end")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.machine
                    .start_tape(tape, delay, stop_on_end, path.display().to_string())
                    .map_err(bad_params)?;
                Ok(json!({"status": "completed", "input_tape": tape_json(&self.machine)}))
            }
            other => Err(bad_params(format!(
                "unknown action {other}; use start, stop or status"
            ))),
        }
    }

    fn run_to_poll(
        &mut self,
        params: &Map<String, Value>,
        cancel: Option<&AtomicBool>,
    ) -> HostResult {
        let target = required_u64(params, "poll")?;
        let current = self.machine.input_polls();
        if target <= current {
            return Err(bad_params(format!(
                "poll {target} is not ahead of the current poll count {current}"
            )));
        }
        let count = target - current;
        if count > MAX_SYNC_ADVANCE_COUNT {
            return Err(bad_params(format!(
                "poll {target} is more than {MAX_SYNC_ADVANCE_COUNT} polls ahead"
            )));
        }
        let mut value = self.advance_json(Unit::InputPolls, count, cancel);
        let flip = params
            .get("until_display_flip")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if flip && value["status"] == "completed" {
            let control = AdvanceControl {
                deadline: Instant::now() + ADVANCE_BUDGET,
                cancel,
            };
            let stop = self.machine.advance_to_display_flip(120, &control);
            value["display_flip"] = json!(match stop {
                Stop::Completed => "reached",
                Stop::Interrupted(reason) => reason,
            });
            if stop != Stop::Completed {
                value["status"] = json!("interrupted");
            }
            value["frame"] = json!(self.machine.frame());
            value["pc"] = json!(self.machine.cpu().pc());
            value["boundary"] = json!(self.machine.boundary().as_str());
        }
        value["target_poll"] = json!(target);
        Ok(value)
    }
}

// --- static capability documents ----------------------------------------------

fn breakpoint_kinds() -> Value {
    json!([{
        "kind": "exec",
        "range_mode": "inclusive",
        "range_unit": "address",
        "memory_type_used": false,
        "pause_on_hit": [true],
        "snapshot": false,
        "snapshot_timing": "pre_instruction",
    }])
}

fn execution_limits() -> Value {
    json!({
        "max_sync_advance_count": MAX_SYNC_ADVANCE_COUNT,
        "max_sync_operation_ms": ADVANCE_BUDGET.as_millis() as u64,
        "frame": {"max_count": MAX_SYNC_ADVANCE_COUNT},
        "max_memory_transfer": MAX_MEMORY_TRANSFER,
    })
}

fn input_axes() -> Value {
    let axis = |negative: &str, positive: &str| json!({"minimum": 0, "neutral": 128, "maximum": 255, "negative_direction": negative, "positive_direction": positive});
    let ports: Vec<Value> = [0, 1]
        .iter()
        .map(|port| {
            json!({
                "port": port,
                "axes": {
                    "left_x": axis("left", "right"),
                    "left_y": axis("up", "down"),
                    "right_x": axis("left", "right"),
                    "right_y": axis("up", "down"),
                },
            })
        })
        .collect();
    json!({
        "semantics": "persistent_override_dualshock_bytes",
        "ports": ports,
        "notes": "Raw DualShock stick bytes; only reported to the guest while the pad is in analog mode (status.input_override.ports[].pad_mode). Omitted axes keep their last value; set_input with no buttons and no axes releases the port to a neutral pad.",
    })
}

fn capability_notes() -> Value {
    json!({
        "implemented_methods": METHODS,
        "extension_methods": EXTENSION_METHODS,
        "step_units": ["frames", "instructions", "input_polls"],
        "step_cpus": [CPU_ID],
        "frame_clock": "route_tick: one vblank_period cycle budget capped at 1,000,000 instructions, the GUI step_one_frame / headless launch route-tick clock",
        "input_poll_clock": "Bus::port1_completed_polls; step_input_polls stops on the instruction that completes the Nth poll (launch --stop-at-poll)",
        "execution_model": "in_process_host: every request runs on the emulator thread between frames, so reads while running observe a frame boundary",
        "save_state_format": "PSoXide SaveStateV1 (GUI / launch --savestate compatible); BIOS and disc come from the running machine",
        "reset": "power-on reboot of the booted content; persistent input overrides survive",
        "memory": "ram/scratchpad/bios/vram are raw device storage; cpu is the R3000A virtual map restricted to RAM, scratchpad and BIOS (I/O registers are refused rather than echoed); bios and vram are read-only",
        "input_tapes": "psoxide_input_tape {action:start,path,delay_ticks?,stop_on_end?} replays PXITAPE1/PXITAPE2/CSV tapes on port 0 with launch --input-tape clocks",
        "hashes": "psoxide_hashes returns the launch --dump-hash vram/display FNV-1a-64 values",
        "breakpoints": "exec (PC) breakpoints over inclusive ranges, matched with segment bits masked; armed breakpoints make every advance single-step and check the PC before each instruction; read/write watchpoints are not exposed",
        "display": "headless; no window, no audio output",
    })
}

// --- parameter helpers ----------------------------------------------------------

fn require_cpu(params: &Map<String, Value>) -> Result<(), (&'static str, String)> {
    match params.get("cpu") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(cpu))
            if cpu.eq_ignore_ascii_case(CPU_ID)
                || ["cpu", "main", "maincpu", "mips"]
                    .iter()
                    .any(|a| cpu.eq_ignore_ascii_case(a)) =>
        {
            Ok(())
        }
        Some(other) => Err(bad_params(format!(
            "unknown cpu target {other}; available: {CPU_ID}"
        ))),
    }
}

fn required_u64(params: &Map<String, Value>, key: &str) -> Result<u64, (&'static str, String)> {
    optional_u64(params, key)?.ok_or_else(|| bad_params(format!("missing required param: {key}")))
}

fn optional_u64(
    params: &Map<String, Value>,
    key: &str,
) -> Result<Option<u64>, (&'static str, String)> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| bad_params(format!("{key} must be an unsigned integer"))),
    }
}

fn memory_type_param(params: &Map<String, Value>) -> Result<MemoryType, (&'static str, String)> {
    let name = params
        .get("memory_type")
        .and_then(Value::as_str)
        .ok_or_else(|| bad_params("missing required param: memory_type"))?;
    MemoryType::from_name(name).ok_or_else(|| {
        bad_params(format!(
            "unknown memory_type {name}; available: {}",
            memory::type_names().join(", ")
        ))
    })
}

fn port_param(
    params: &Map<String, Value>,
    machine: &Machine,
) -> Result<usize, (&'static str, String)> {
    match optional_u64(params, "port")?.unwrap_or(0) {
        0 => Ok(0),
        1 if machine.port2_connected() => Ok(1),
        1 => Err(bad_state(
            "no controller on port 1 (the second port); launch with --pad2",
        )),
        other => Err(bad_params(format!("port must be 0 or 1, got {other}"))),
    }
}

fn decode_buttons(value: Option<&Value>) -> Result<u16, (&'static str, String)> {
    let values = value
        .and_then(Value::as_array)
        .ok_or_else(|| bad_params("buttons must be an array"))?;
    let mut mask = 0u16;
    for value in values {
        let name = value
            .as_str()
            .ok_or_else(|| bad_params("button names must be strings"))?;
        mask |= button_bit(name)
            .ok_or_else(|| bad_params(format!("unsupported PSX button: {name}")))?;
    }
    Ok(mask)
}

/// `None` = release. Omitted axes keep the current override's value.
fn decode_input(
    params: &Map<String, Value>,
    current: PortInput,
) -> Result<Option<PortInput>, (&'static str, String)> {
    let buttons = decode_buttons(params.get("buttons"))?;
    let axes = match params.get("axes") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(axes)) => axes.clone(),
        Some(_) => return Err(bad_params("axes must be an object of axis name -> 0..255")),
    };
    if buttons == 0 && axes.is_empty() {
        return Ok(None);
    }
    let mut sticks = if current.engaged {
        current.sticks
    } else {
        [0x80; 4]
    };
    for (name, value) in &axes {
        let slot = match name.as_str() {
            "right_x" => 0,
            "right_y" => 1,
            "left_x" => 2,
            "left_y" => 3,
            other => {
                return Err(bad_params(format!(
                    "unknown axis {other}; use left_x, left_y, right_x, right_y"
                )))
            }
        };
        sticks[slot] = value
            .as_u64()
            .filter(|v| *v <= 255)
            .ok_or_else(|| bad_params(format!("axis {name} must be 0..=255")))?
            as u8;
    }
    Ok(Some(PortInput {
        buttons,
        sticks,
        engaged: true,
    }))
}

fn requested_path(params: &Map<String, Value>) -> Result<PathBuf, (&'static str, String)> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| bad_params("missing or invalid param: path"))?;
    if path.is_absolute() {
        Ok(path)
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|e| ("io_error", e.to_string()))
    }
}

// --- JSON fragments --------------------------------------------------------------

fn axes_json(sticks: [u8; 4]) -> Value {
    json!({"right_x": sticks[0], "right_y": sticks[1], "left_x": sticks[2], "left_y": sticks[3]})
}

fn pad_mode(machine: &Machine, port: usize) -> Value {
    let mode = if port == 0 {
        machine.bus().port1_pad_mode()
    } else {
        machine.bus().port2_pad_mode()
    };
    mode.map_or(Value::Null, |mode| {
        json!(format!("{mode:?}").to_ascii_lowercase())
    })
}

fn input_override_json(machine: &Machine) -> Value {
    let ports: Vec<Value> = (0..2)
        .filter(|&port| port == 0 || machine.port2_connected())
        .map(|port| {
            let input = machine.port_input(port);
            json!({
                "port": port,
                "engaged": input.engaged,
                "ownership": if port == 0 && machine.tape().is_some() { "input_tape" } else if input.engaged { "override" } else { "neutral" },
                "buttons": button_names(input.buttons),
                "pressed_mask": input.buttons,
                "axes": axes_json(input.sticks),
                "pad_mode": pad_mode(machine, port),
            })
        })
        .collect();
    json!({"observable": true, "ports": ports})
}

fn tape_json(machine: &Machine) -> Value {
    let Some(tape) = machine.tape() else {
        return Value::Null;
    };
    json!({
        "source": tape.source,
        "clock": match tape.tape.clock {
            emulator_core::input_tape::TapeClock::VideoFrame => "video_frame",
            emulator_core::input_tape::TapeClock::PadPoll => "pad_poll",
        },
        "samples": tape.tape.samples.len(),
        "start_poll": tape.tape.start_poll,
        "start_frame": tape.start_frame,
        "delay_ticks": tape.delay_ticks,
        "cursor": tape.cursor,
        "stop_on_end": tape.stop_on_end,
        "game_hash": tape.tape.game_hash.map(|hash| format!("{hash:016x}")),
    })
}

// --- byte helpers -----------------------------------------------------------------

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

pub(crate) fn parse_hex(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) {
        return Err("hex must have an even number of digits".into());
    }
    (0..text.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&text[i..i + 2], 16)
                .map_err(|_| "hex contains a non-hex digit".to_string())
        })
        .collect()
}

fn vram_rgba(machine: &Machine) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(1024 * 512 * 4);
    for &px in machine.bus().gpu.vram.words().iter().take(1024 * 512) {
        let expand = |c: u16| {
            let c = (c & 0x1f) as u8;
            (c << 3) | (c >> 2)
        };
        rgba.extend_from_slice(&[expand(px), expand(px >> 5), expand(px >> 10), 255]);
    }
    rgba
}

fn encode_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let image = image::RgbaImage::from_raw(width, height, rgba.to_vec())
        .ok_or("rgba buffer does not match its dimensions")?;
    let mut png = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut png, image::ImageFormat::Png)
        .map_err(|error| format!("png encode: {error}"))?;
    Ok(png.into_inner())
}

fn hash_file(path: &Path) -> Result<(String, String, u64), String> {
    use std::io::Read as _;
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut sha1 = sha1::Sha1::new();
    let mut sha256 = sha2::Sha256::new();
    let mut size = 0u64;
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        if read == 0 {
            break;
        }
        sha1.update(&buffer[..read]);
        sha256.update(&buffer[..read]);
        size += read as u64;
    }
    Ok((hex(&sha1.finalize()), hex(&sha256.finalize()), size))
}

/// Write via a sibling temp file and a rename, so a failed save never leaves a
/// truncated state where an older one used to be.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("state");
    let partial = parent.join(format!(".{name}.partial.{}", std::process::id()));
    std::fs::write(&partial, bytes).map_err(|e| format!("write {}: {e}", partial.display()))?;
    std::fs::rename(&partial, path).map_err(|e| {
        let _ = std::fs::remove_file(&partial);
        format!("rename into {}: {e}", path.display())
    })
}
