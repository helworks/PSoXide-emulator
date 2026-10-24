//! End-to-end tests of the emucap host against a real `Bus` + `Cpu`.
//!
//! No disc or BIOS is needed: the guest is a hand-assembled PSX-EXE that polls
//! the port-1 pad over SIO0 exactly the way a game's pad driver does, stores the
//! wire button word and a poll counter in RAM, then idles. That gives every
//! clock the host exposes -- frames, instructions and controller polls -- an
//! observable, deterministic guest-side effect.

use psoxide_emucap::host::{Host, HostIdentity, METHODS};
use psoxide_emucap::machine::{
    AdvanceControl, BootConfig, Content, Machine, PadProfile, Stop, Unit,
};
use psoxide_emucap::wire::{Request, Response};
use serde_json::{json, Value};

const BASE: u32 = 0x8001_0000;
/// Guest poll counter (RAM offset).
const COUNTER: u64 = 0x2_0000;
/// Guest copy of the last polled 16-bit button word, as on the wire (active-low).
const WIRE: u64 = 0x2_0004;

// --- a tiny MIPS assembler -------------------------------------------------

const ZERO: u32 = 0;
const T0: u32 = 8;
const T1: u32 = 9;
const T2: u32 = 10;
const T3: u32 = 11;
const T4: u32 = 12;
const T5: u32 = 13;
const T6: u32 = 14;
const T7: u32 = 15;
const S0: u32 = 16;
const RA: u32 = 31;

fn i(op: u32, rs: u32, rt: u32, imm: u32) -> u32 {
    (op << 26) | (rs << 21) | (rt << 16) | (imm & 0xffff)
}
fn r(rs: u32, rt: u32, rd: u32, sa: u32, funct: u32) -> u32 {
    (rs << 21) | (rt << 16) | (rd << 11) | (sa << 6) | funct
}
fn at(index: u32) -> u32 {
    BASE + index * 4
}
fn branch(op: u32, rs: u32, rt: u32, from: u32, to: u32) -> u32 {
    let offset = (at(to) as i64 - (at(from) as i64 + 4)) / 4;
    i(op, rs, rt, offset as u32)
}
fn jump(op: u32, to: u32) -> u32 {
    (op << 26) | ((at(to) >> 2) & 0x03ff_ffff)
}

/// The pad-polling guest. Instruction indices are referenced by the branches.
fn guest_program() -> Vec<u32> {
    let xchg = 33;
    let program = vec![
        /* 0 */ i(0x0f, 0, T2, 0x1f80), // lui t2, 0x1f80
        /* 1 */ i(0x0d, T2, T2, 0x1040), // ori t2, t2, 0x1040 (SIO0)
        /* 2 */ i(0x0f, 0, T0, 0x8002), // lui t0, 0x8002 (counter base)
        /* 3 main */ i(0x0d, ZERO, T3, 2), // ori t3, zero, 2
        /* 4 */ i(0x29, T2, T3, 0xa), // sh t3, 0xa(t2): select port 1
        /* 5 */ i(0x0d, ZERO, T4, 0x01),
        /* 6 */ jump(0x03, xchg),
        /* 7 */ 0,
        /* 8 */ i(0x0d, ZERO, T4, 0x42),
        /* 9 */ jump(0x03, xchg),
        /* 10 */ 0,
        /* 11 */ i(0x0d, ZERO, T4, 0x00),
        /* 12 */ jump(0x03, xchg), // id high
        /* 13 */ 0,
        /* 14 */ jump(0x03, xchg), // buttons low
        /* 15 */ 0,
        /* 16 */ r(T6, ZERO, S0, 0, 0x25), // or s0, t6, zero
        /* 17 */ jump(0x03, xchg), // buttons high
        /* 18 */ 0,
        /* 19 */ i(0x29, T2, ZERO, 0xa), // sh zero, 0xa(t2): deselect
        /* 20 */ r(0, T6, T6, 8, 0x00), // sll t6, t6, 8
        /* 21 */ r(S0, T6, S0, 0, 0x25), // or s0, s0, t6
        /* 22 */ i(0x2b, T0, S0, 4), // sw s0, 4(t0)
        /* 23 */ i(0x23, T0, T1, 0), // lw t1, 0(t0)
        /* 24 */ 0,
        /* 25 */ i(0x09, T1, T1, 1), // addiu t1, t1, 1
        /* 26 */ i(0x2b, T0, T1, 0), // sw t1, 0(t0)
        /* 27 */ i(0x0f, 0, T7, 0x0001), // lui t7, 1 (delay loop count)
        /* 28 delay */ i(0x09, T7, T7, 0xffff), // addiu t7, t7, -1
        /* 29 */ branch(0x05, T7, ZERO, 29, 28), // bne t7, zero, delay
        /* 30 */ 0,
        /* 31 */ jump(0x02, 3), // j main
        /* 32 */ 0,
        /* 33 xchg */ i(0x28, T2, T4, 0), // sb t4, 0(t2)
        /* 34 wait */ i(0x25, T2, T5, 4), // lhu t5, 4(t2): JOY_STAT
        /* 35 */ 0,
        /* 36 */ i(0x0c, T5, T5, 2), // andi t5, t5, 2: RX not empty
        /* 37 */ branch(0x04, T5, ZERO, 37, 34), // beq t5, zero, wait
        /* 38 */ 0,
        /* 39 */ i(0x24, T2, T6, 0), // lbu t6, 0(t2)
        /* 40 */ r(RA, 0, 0, 0, 0x08), // jr ra
        /* 41 */ 0,
    ];
    assert_eq!(program.len(), 42);
    program
}

fn guest_exe() -> Vec<u8> {
    let payload: Vec<u8> = guest_program()
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .collect();
    let mut exe = vec![0u8; 0x800];
    exe[..8].copy_from_slice(b"PS-X EXE");
    exe[0x10..0x14].copy_from_slice(&BASE.to_le_bytes());
    exe[0x18..0x1c].copy_from_slice(&BASE.to_le_bytes());
    exe[0x1c..0x20].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    exe[0x30..0x34].copy_from_slice(&0x801f_fff0u32.to_le_bytes());
    exe.extend_from_slice(&payload);
    exe
}

// --- harness -----------------------------------------------------------------

fn host_with(pad: PadProfile, pad2: bool) -> Host {
    let mut config = BootConfig::new(Content::ExeBytes {
        label: "pad-poll-guest.exe".into(),
        bytes: guest_exe(),
    });
    config.pad = pad;
    config.pad2 = pad2;
    let machine = Machine::boot(config).expect("boot synthetic EXE");
    Host::new(
        machine,
        HostIdentity {
            start_frozen: true,
            ..Default::default()
        },
    )
}

fn host() -> Host {
    host_with(PadProfile::Digital, false)
}

fn call(host: &mut Host, method: &str, params: Value) -> Response {
    let line = json!({"v": 1, "id": 42, "method": method, "params": params}).to_string();
    let request = Request::parse(&line).expect("well-formed request");
    let response = host.handle(&request, None);
    assert_eq!(response.id, 42);
    response
}

fn ok(host: &mut Host, method: &str, params: Value) -> Value {
    let response = call(host, method, params);
    assert!(response.ok, "{method} failed: {:?}", response.error);
    response.result.unwrap()
}

fn err_kind(host: &mut Host, method: &str, params: Value) -> String {
    let response = call(host, method, params);
    assert!(
        !response.ok,
        "{method} unexpectedly succeeded: {:?}",
        response.result
    );
    response.error.unwrap().kind
}

fn read_u32(host: &mut Host, offset: u64) -> u32 {
    let result = ok(
        host,
        "read_memory",
        json!({"memory_type": "ram", "address": offset, "length": 4}),
    );
    let hex = result["hex"].as_str().unwrap();
    let bytes: Vec<u8> = (0..4)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
        .collect();
    u32::from_le_bytes(bytes.try_into().unwrap())
}

fn wire_word(host: &mut Host) -> u16 {
    read_u32(host, WIRE) as u16
}

/// Let the guest finish the poll transaction that just completed and store
/// what it read: step until its poll counter moves (it then sits in the
/// ~200k-instruction idle loop, far from the next poll).
fn settle(host: &mut Host) {
    let before = read_u32(host, COUNTER);
    for _ in 0..200 {
        ok(host, "step_instructions", json!({"count": 100}));
        if read_u32(host, COUNTER) != before {
            return;
        }
    }
    panic!("the guest never stored its poll result");
}

fn scratch_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("psoxide-emucap-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// --- discovery -----------------------------------------------------------------

/// Method names with a feature contract in emucap's `contracts/catalog.json`
/// (v3). An advertised method outside this list makes emucap mark the whole
/// contract advertisement `unvalidated`.
const CATALOG_METHODS: &[&str] = &[
    "hello",
    "status",
    "get_rom_info",
    "read_memory",
    "read_memory_batch",
    "execution_speed",
    "write_memory",
    "find_pattern",
    "dump_memory",
    "get_state",
    "screenshot",
    "get_video_state",
    "resolve_tile",
    "set_layer_enable",
    "set_input",
    "press_buttons",
    "touch",
    "pause",
    "resume",
    "run_frames",
    "step",
    "step_input_polls",
    "read_input_polls",
    "step_instructions",
    "reset",
    "power_cycle",
    "save_state",
    "capture_snapshot",
    "observe_snapshot_halt",
    "load_state",
    "probe",
    "set_breakpoint",
    "clear_breakpoint",
    "list_breakpoints",
    "clear_all_breakpoints",
    "poll_events",
    "record_window",
    "disassemble",
    "watch_register",
    "set_trace",
    "get_trace",
    "call_stack",
    "break_on_reset",
    "cancel_operation",
    "begin_temporal_operation",
    "finish_temporal_operation",
    "change_media",
];

#[test]
fn hello_is_acceptable_to_emucaps_link_validation() {
    let mut host = host();
    let hello = ok(&mut host, "hello", json!({}));
    assert_eq!(hello["protocol_version"], 1);
    assert_eq!(hello["system"], "psx");
    assert!(
        hello.get("session_token").is_none(),
        "hello must never disclose credentials"
    );
    let methods: Vec<&str> = hello["methods"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap())
        .collect();
    for method in &methods {
        assert!(
            CATALOG_METHODS.contains(method),
            "{method} has no emucap feature contract"
        );
    }
    let memory_types: Vec<&str> = hello["memory_types"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap())
        .collect();
    for region in hello["memory_regions"].as_array().unwrap() {
        assert!(memory_types.contains(&region["memory_type"].as_str().unwrap()));
        assert!(region["size"].as_u64().unwrap() > 0);
    }
    // read_memory_batch and memory_batch_capability are advertised together,
    // with emucap's core limits and windows over advertised types only.
    assert!(methods.contains(&"read_memory_batch"));
    let batch = &hello["memory_batch_capability"];
    let max_ranges = batch["max_ranges"].as_u64().unwrap();
    assert!((2..=64).contains(&max_ranges));
    assert!(batch["max_total_bytes"].as_u64().unwrap() <= 64 * 1024);
    assert!(
        batch["max_range_bytes"].as_u64().unwrap() <= batch["max_total_bytes"].as_u64().unwrap()
    );
    assert_eq!(batch["consistency"], "frozen_boundary");
    for window in batch["windows"].as_array().unwrap() {
        assert!(memory_types.contains(&window["memory_type"].as_str().unwrap()));
    }
    // Paired capabilities emucap rejects when only one half is present.
    assert!(
        !methods.contains(&"execution_speed") && hello.get("execution_speed_capability").is_none()
    );
    assert!(
        !methods.contains(&"cancel_operation")
            && hello.get("temporal_cancellation_capability").is_none()
    );
    let defaults = hello["cpu_targets"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["default"] == true)
        .count();
    assert_eq!(defaults, 1);
    assert_eq!(hello["contracts"]["catalog"], "emucap-feature-contracts/v3");
    assert_eq!(
        hello["contracts"]["active_exceptions"],
        json!(psoxide_emucap::host::ACTIVE_EXCEPTIONS)
    );
}

#[test]
fn every_advertised_method_is_callable() {
    let dir = scratch_dir("callable");
    let state = dir.join("callable.psxstate");
    let mut host = host();
    for method in METHODS {
        let params = match *method {
            "read_memory" => json!({"memory_type": "ram", "address": 0, "length": 4}),
            "write_memory" => json!({"memory_type": "ram", "address": 0x3_0000, "hex": "00"}),
            "read_memory_batch" => {
                json!({"ranges": [{"memory_type": "ram", "address": 0, "length": 4}]})
            }
            "set_input" => json!({"buttons": []}),
            "press_buttons" => json!({"buttons": ["start"], "frames": 1}),
            "save_state" | "load_state" => json!({"path": state.display().to_string()}),
            "screenshot" => json!({"source": "vram"}),
            "set_breakpoint" => json!({"kind": "exec", "start": BASE}),
            "clear_breakpoint" => json!({"id": 1}),
            _ => json!({}),
        };
        let response = call(&mut host, method, params);
        assert!(response.ok, "{method}: {:?}", response.error);
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn unknown_and_reserved_requests_fail_explicitly() {
    let mut host = host();
    assert_eq!(
        err_kind(&mut host, "no_such_method", json!({})),
        "unsupported"
    );
    assert_eq!(
        err_kind(&mut host, "step", json!({"_temporal_owner": {}})),
        "unsupported"
    );
    assert_eq!(
        err_kind(&mut host, "step", json!({"frames": 0})),
        "bad_params"
    );
    assert_eq!(
        err_kind(&mut host, "step", json!({"frames": 5001})),
        "bad_params"
    );
    assert_eq!(
        err_kind(&mut host, "get_state", json!({"cpu": "gte"})),
        "bad_params"
    );
    let status = ok(&mut host, "status", json!({}));
    assert_eq!(
        status["frame"], 0,
        "rejected requests must not advance the guest"
    );
    assert_eq!(status["boundary"], "pre_first_instruction");
}

// --- clocks ----------------------------------------------------------------------

#[test]
fn frame_steps_are_exact_and_end_frozen() {
    let mut host = host();
    let step = ok(&mut host, "step", json!({"frames": 7}));
    assert_eq!(step["status"], "completed");
    assert_eq!(step["unit"], "frames");
    assert_eq!(step["completed"], 7);
    assert_eq!(step["frame"], 7);
    assert_eq!(step["state"], "frozen");
    assert_eq!(step["boundary"], "frame_boundary");
    let status = ok(&mut host, "status", json!({}));
    assert_eq!(status["frame"], 7);
    assert_eq!(status["state"], "frozen");
    assert!(
        read_u32(&mut host, COUNTER) > 0,
        "the guest polled during those frames"
    );
}

#[test]
fn instruction_steps_retire_exactly_n_instructions() {
    let mut host = host();
    let before = ok(&mut host, "get_state", json!({}));
    assert_eq!(before["state"]["cpu.pc"], BASE);
    let step = ok(&mut host, "step_instructions", json!({"count": 3}));
    assert_eq!(step["completed"], 3);
    assert_eq!(step["cpu_tick_delta"], 3);
    assert_eq!(step["boundary"], "instruction_boundary");
    let after = ok(&mut host, "get_state", json!({"groups": ["cpu"]}));
    assert_eq!(after["state"]["cpu.pc"], at(3));
    assert_eq!(
        after["state"]["cpu.r10"], 0x1f80_1040u32,
        "lui+ori built the SIO0 base"
    );
    assert!(
        after["state"].get("cop0.sr").is_none(),
        "group filter applied"
    );
}

#[test]
fn input_poll_steps_stop_on_the_completing_instruction() {
    let mut host = host();
    let first = ok(&mut host, "step_input_polls", json!({"count": 1}));
    assert_eq!(first["status"], "completed");
    assert_eq!(first["input_polls_after"], 1);
    // The guest is still inside its pad driver: the counter store lies ahead.
    assert_eq!(read_u32(&mut host, COUNTER), 0);
    let three = ok(&mut host, "step_input_polls", json!({"count": 3}));
    assert_eq!(three["input_polls_before"], 1);
    assert_eq!(three["input_polls_after"], 4);
    assert_eq!(three["completed"], 3);
    assert_eq!(
        read_u32(&mut host, COUNTER),
        3,
        "exactly one guest store per completed poll"
    );
}

fn guest_machine() -> Machine {
    let mut config = BootConfig::new(Content::ExeBytes {
        label: "pad-poll-guest.exe".into(),
        bytes: guest_exe(),
    });
    config.pad = PadProfile::Digital;
    Machine::boot(config).unwrap()
}

fn control() -> AdvanceControl<'static> {
    AdvanceControl {
        deadline: std::time::Instant::now() + std::time::Duration::from_secs(120),
        cancel: None,
    }
}

#[test]
fn batched_poll_stops_land_on_the_same_instruction_as_single_stepping() {
    let mut batched = guest_machine();
    let mut exact = guest_machine();
    exact.set_exact_poll_stepping(true);
    let mut stepped = guest_machine();
    for target in 1..=4u64 {
        assert_eq!(
            batched.advance(Unit::InputPolls, 1, &control()).stop,
            Stop::Completed
        );
        assert_eq!(
            exact.advance(Unit::InputPolls, 1, &control()).stop,
            Stop::Completed
        );
        while stepped.input_polls() < target {
            stepped.advance(Unit::Instructions, 1, &control());
        }
        for other in [&exact, &stepped] {
            assert_eq!(
                batched.cpu().tick(),
                other.cpu().tick(),
                "poll {target}: instruction count"
            );
            assert_eq!(batched.cpu().pc(), other.cpu().pc(), "poll {target}: pc");
            assert_eq!(
                batched.bus().cycles(),
                other.bus().cycles(),
                "poll {target}: bus clock"
            );
        }
    }
}

#[test]
fn a_raised_cancel_flag_stops_an_advance_before_it_starts() {
    let mut machine = guest_machine();
    let cancel = std::sync::atomic::AtomicBool::new(true);
    let advance = machine.advance(
        Unit::Frames,
        100,
        &AdvanceControl {
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(60),
            cancel: Some(&cancel),
        },
    );
    assert_eq!(advance.stop, Stop::Interrupted("cancelled"));
    assert_eq!(advance.completed, 0);
    assert_eq!(machine.frame(), 0);
    let expired = machine.advance(
        Unit::Frames,
        100,
        &AdvanceControl {
            deadline: std::time::Instant::now(),
            cancel: None,
        },
    );
    assert_eq!(expired.stop, Stop::Interrupted("host_deadline"));
}

#[test]
fn a_guest_that_stops_polling_is_reported_not_waited_on() {
    let mut host = host();
    // Turn the entry point into `j entry; nop`: the guest never touches SIO0.
    let spin = format!("{}00000000", hex_word(jump(0x02, 0)));
    ok(
        &mut host,
        "write_memory",
        json!({"memory_type": "cpu", "address": BASE, "hex": spin}),
    );
    let step = ok(&mut host, "step_input_polls", json!({"count": 1}));
    assert_eq!(step["status"], "interrupted");
    assert_eq!(step["reason"], "no_input_poll");
    assert_eq!(step["completed"], 0);
    assert_eq!(step["state"], "frozen");
}

#[test]
fn exec_breakpoints_stop_before_the_instruction_and_resume_past_it() {
    let mut host = host();
    let set = ok(
        &mut host,
        "set_breakpoint",
        json!({"kind": "exec", "start": at(23)}),
    );
    let id = set["id"].as_u64().unwrap();
    // A KUSEG alias of the same instruction is the same breakpoint target.
    assert_eq!(
        err_kind(
            &mut host,
            "set_breakpoint",
            json!({"kind": "read", "start": 0})
        ),
        "bad_params"
    );

    let hit = ok(&mut host, "step", json!({"frames": 30}));
    assert_eq!(hit["status"], "interrupted");
    assert_eq!(hit["reason"], "breakpoint");
    assert_eq!(hit["breakpoint_id"], id);
    assert_eq!(hit["pc"], at(23), "stopped before the instruction ran");
    assert_eq!(hit["state"], "frozen");
    assert_eq!(
        read_u32(&mut host, COUNTER),
        0,
        "the counter load/store has not run"
    );
    let events = ok(&mut host, "poll_events", json!({}));
    assert_eq!(events["events"][0]["type"], "breakpoint_hit");
    assert_eq!(events["events"][0]["snapshot_timing"], "pre_instruction");

    // Resuming executes the stopped instruction, then stops on the next pass.
    let again = ok(&mut host, "step", json!({"frames": 30}));
    assert_eq!(again["reason"], "breakpoint");
    assert_eq!(
        read_u32(&mut host, COUNTER),
        1,
        "exactly one loop pass between hits"
    );
    let listed = ok(&mut host, "list_breakpoints", json!({}));
    assert_eq!(listed["breakpoints"][0]["hits"], 2);

    // Free running freezes on a hit too.
    ok(&mut host, "resume", json!({}));
    for _ in 0..30 {
        host.run_free_frame();
    }
    assert_eq!(ok(&mut host, "status", json!({}))["state"], "frozen");

    assert_eq!(
        ok(&mut host, "clear_all_breakpoints", json!({}))["cleared"],
        1
    );
    assert_eq!(
        err_kind(&mut host, "clear_breakpoint", json!({"id": id})),
        "bad_params"
    );
    let free = ok(&mut host, "step", json!({"frames": 5}));
    assert_eq!(free["status"], "completed");
}

#[test]
fn breakpoints_match_across_segment_aliases() {
    let mut host = host();
    ok(
        &mut host,
        "set_breakpoint",
        json!({"start": at(23) & 0x1fff_ffff}),
    );
    let hit = ok(&mut host, "step", json!({"frames": 30}));
    assert_eq!(hit["reason"], "breakpoint");
    assert_eq!(hit["pc"], at(23));
}

fn hex_word(word: u32) -> String {
    word.to_le_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn identical_requests_give_identical_machines() {
    let run = || {
        let mut host = host();
        ok(&mut host, "set_input", json!({"buttons": ["cross", "up"]}));
        ok(&mut host, "step", json!({"frames": 12}));
        ok(&mut host, "step_input_polls", json!({"count": 2}));
        ok(&mut host, "step_instructions", json!({"count": 17}));
        let hashes = ok(&mut host, "psoxide_hashes", json!({}));
        let ram = ok(
            &mut host,
            "read_memory",
            json!({"memory_type": "ram", "address": 0x2_0000, "length": 8}),
        );
        let state = ok(&mut host, "get_state", json!({}));
        (
            hashes["vram_fnv1a_64"].clone(),
            ram["hex"].clone(),
            state["state"].clone(),
        )
    };
    assert_eq!(run(), run());
}

// --- input -----------------------------------------------------------------------

#[test]
fn set_input_reaches_the_wire_the_guest_polls() {
    use emulator_core::button::{CROSS, START, UP};
    let mut host = host();
    ok(&mut host, "step_input_polls", json!({"count": 1}));
    settle(&mut host);
    assert_eq!(wire_word(&mut host), 0xffff, "released pad reads all-high");

    let set = ok(
        &mut host,
        "set_input",
        json!({"port": 0, "buttons": ["cross", "start", "up"]}),
    );
    assert_eq!(set["override_engaged"], true);
    assert_eq!(set["applied_mask"], format!("0x{:04x}", CROSS | START | UP));
    ok(&mut host, "step_input_polls", json!({"count": 1}));
    settle(&mut host);
    assert_eq!(wire_word(&mut host), !(CROSS | START | UP));

    let release = ok(&mut host, "set_input", json!({"buttons": []}));
    assert_eq!(release["override_engaged"], false);
    ok(&mut host, "step_input_polls", json!({"count": 1}));
    settle(&mut host);
    assert_eq!(wire_word(&mut host), 0xffff);
}

#[test]
fn press_buttons_pulses_then_restores_the_persistent_override() {
    use emulator_core::button::{CIRCLE, L1};
    let mut host = host();
    ok(&mut host, "set_input", json!({"buttons": ["l1"]}));
    let pressed = ok(
        &mut host,
        "press_buttons",
        json!({"buttons": ["o"], "frames": 4}),
    );
    assert_eq!(pressed["status"], "completed");
    assert_eq!(pressed["frames_completed"], 4);
    assert_eq!(pressed["transient_override_engaged"], false);
    assert_eq!(pressed["state"], "frozen");
    // During the pulse the guest saw L1 + circle ...
    assert_eq!(wire_word(&mut host) & (CIRCLE | L1), 0);
    // ... afterwards only the persistent L1.
    ok(&mut host, "step_input_polls", json!({"count": 1}));
    settle(&mut host);
    assert_eq!(wire_word(&mut host), !L1);
    let status = ok(&mut host, "status", json!({}));
    assert_eq!(
        status["input_override"]["ports"][0]["buttons"],
        json!(["l1"])
    );
    assert_eq!(
        err_kind(&mut host, "press_buttons", json!({"buttons": ["nope"]})),
        "bad_params"
    );
    assert_eq!(
        err_kind(&mut host, "set_input", json!({"port": 1, "buttons": ["x"]})),
        "bad_state"
    );
}

#[test]
fn second_port_is_drivable_when_plugged() {
    let mut host = host_with(PadProfile::Digital, true);
    let set = ok(
        &mut host,
        "set_input",
        json!({"port": 1, "buttons": ["square"]}),
    );
    assert_eq!(set["port"], 1);
    let status = ok(&mut host, "status", json!({}));
    assert_eq!(
        status["input_override"]["ports"][1]["buttons"],
        json!(["square"])
    );
}

#[test]
fn video_frame_and_poll_bound_tapes_drive_port_one() {
    use emulator_core::button::{CROSS, SQUARE, TRIANGLE};
    use emulator_core::input_tape::{write_tape, write_tape_poll_bound, PadSample};
    let dir = scratch_dir("tapes");

    // Poll-bound: sample N reaches poll start+N whatever the frame rate.
    let mut host = host();
    ok(&mut host, "step_input_polls", json!({"count": 2}));
    let poll_tape = dir.join("route.pxtape");
    write_tape_poll_bound(
        &poll_tape,
        &[
            PadSample::from_buttons(CROSS),
            PadSample::from_buttons(SQUARE),
            PadSample::from_buttons(TRIANGLE),
        ],
        2,
        None,
    )
    .unwrap();
    let started = ok(
        &mut host,
        "psoxide_input_tape",
        json!({"action": "start", "path": poll_tape.display().to_string(), "stop_on_end": true}),
    );
    assert_eq!(started["input_tape"]["clock"], "pad_poll");
    assert_eq!(
        err_kind(&mut host, "set_input", json!({"buttons": ["x"]})),
        "busy"
    );
    for expected in [CROSS, SQUARE] {
        let step = ok(&mut host, "step_input_polls", json!({"count": 1}));
        assert_eq!(step["status"], "completed");
        settle(&mut host);
        assert_eq!(wire_word(&mut host), !expected);
    }
    // The poll that reads the last sample spends the tape; with stop_on_end the
    // advance stops on that same instruction.
    let end = ok(&mut host, "step_input_polls", json!({"count": 1}));
    assert_eq!(end["status"], "interrupted");
    assert_eq!(end["reason"], "input_tape_finished");
    assert_eq!(end["input_polls_after"], 5);
    settle(&mut host);
    assert_eq!(wire_word(&mut host), !TRIANGLE);
    assert!(ok(&mut host, "status", json!({}))["input_tape"].is_null());
    let events = ok(&mut host, "poll_events", json!({}));
    assert_eq!(events["events"][0]["type"], "input_tape_finished");
    assert_eq!(
        ok(&mut host, "poll_events", json!({}))["events"],
        json!([]),
        "events drain once"
    );

    // Video-frame: one sample per route tick, from the frame playback starts.
    let mut host = self::host();
    let frame_tape = dir.join("frames.pxtape");
    let samples: Vec<PadSample> = (0..40)
        .map(|frame| PadSample::from_buttons(if frame < 20 { CROSS } else { SQUARE }))
        .collect();
    write_tape(&frame_tape, &samples).unwrap();
    ok(
        &mut host,
        "psoxide_input_tape",
        json!({"action": "start", "path": frame_tape.display().to_string()}),
    );
    ok(&mut host, "step", json!({"frames": 10}));
    assert_eq!(wire_word(&mut host), !CROSS);
    ok(&mut host, "step", json!({"frames": 15}));
    assert_eq!(wire_word(&mut host), !SQUARE);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn run_to_poll_targets_an_absolute_poll_count() {
    let mut host = host();
    let result = ok(&mut host, "psoxide_run_to_poll", json!({"poll": 5}));
    assert_eq!(result["status"], "completed");
    assert_eq!(result["input_polls_after"], 5);
    assert_eq!(
        err_kind(&mut host, "psoxide_run_to_poll", json!({"poll": 5})),
        "bad_params"
    );
}

// --- memory ------------------------------------------------------------------------

#[test]
fn memory_reads_writes_and_refusals() {
    let mut host = host();
    let code = ok(
        &mut host,
        "read_memory",
        json!({"memory_type": "cpu", "address": BASE, "length": 4}),
    );
    assert_eq!(
        code["hex"], "801f0a3c",
        "lui t2, 0x1f80 at the entry point, little-endian"
    );
    let ram = ok(
        &mut host,
        "read_memory",
        json!({"memory_type": "ram", "address": 0x1_0000, "length": 4}),
    );
    assert_eq!(ram["hex"], code["hex"], "KSEG0 aliases main RAM");

    ok(
        &mut host,
        "write_memory",
        json!({"memory_type": "cpu", "address": 0xa003_0000u32, "hex": "deadbeef"}),
    );
    let back = ok(
        &mut host,
        "read_memory",
        json!({"memory_type": "ram", "address": 0x3_0000, "length": 4}),
    );
    assert_eq!(back["hex"], "deadbeef");
    ok(
        &mut host,
        "write_memory",
        json!({"memory_type": "scratchpad", "address": 0x3fc, "hex": "01020304"}),
    );
    let pad = ok(
        &mut host,
        "read_memory",
        json!({"memory_type": "cpu", "address": 0x1f80_03fcu32, "length": 4}),
    );
    assert_eq!(pad["hex"], "01020304");

    // Refusals change nothing.
    let bios_before = ok(
        &mut host,
        "read_memory",
        json!({"memory_type": "bios", "address": 0, "length": 4}),
    );
    assert_eq!(
        err_kind(
            &mut host,
            "write_memory",
            json!({"memory_type": "bios", "address": 0, "hex": "00"})
        ),
        "unsupported"
    );
    assert_eq!(
        ok(
            &mut host,
            "read_memory",
            json!({"memory_type": "bios", "address": 0, "length": 4})
        )["hex"],
        bios_before["hex"]
    );
    assert_eq!(
        err_kind(
            &mut host,
            "read_memory",
            json!({"memory_type": "cpu", "address": 0x1f80_1810u32, "length": 4})
        ),
        "bad_params"
    );
    assert_eq!(
        err_kind(
            &mut host,
            "read_memory",
            json!({"memory_type": "ram", "address": 0x1f_fffe, "length": 4})
        ),
        "bad_params"
    );
    // The last RAM mirror ends at 8 MiB; a write running past it is refused whole.
    assert_eq!(
        err_kind(
            &mut host,
            "write_memory",
            json!({"memory_type": "cpu", "address": 0x807f_fffeu32, "hex": "aabbccdd"})
        ),
        "unsupported"
    );
    let edge = ok(
        &mut host,
        "read_memory",
        json!({"memory_type": "ram", "address": 0x1f_fffe, "length": 2}),
    );
    assert_eq!(
        edge["hex"], "0000",
        "a refused straddling write left the RAM edge untouched"
    );
    // Inside the mirrors the same bytes alias main RAM.
    ok(
        &mut host,
        "write_memory",
        json!({"memory_type": "cpu", "address": 0x803f_fffeu32, "hex": "aabb"}),
    );
    let alias = ok(
        &mut host,
        "read_memory",
        json!({"memory_type": "ram", "address": 0x1f_fffe, "length": 2}),
    );
    assert_eq!(alias["hex"], "aabb");
}

#[test]
fn memory_batch_is_frozen_exact_and_ordered() {
    let mut host = host();
    ok(&mut host, "step", json!({"frames": 3}));
    let batch = ok(
        &mut host,
        "read_memory_batch",
        json!({"ranges": [
            {"memory_type": "ram", "address": COUNTER, "length": 8},
            {"memory_type": "cpu", "address": BASE, "length": 4},
            {"memory_type": "vram", "address": 0, "length": 16},
        ]}),
    );
    assert_eq!(batch["state"], "frozen");
    assert_eq!(batch["consistency"], "frozen_boundary");
    assert_eq!(batch["total_bytes"], 28);
    let reads = batch["reads"].as_array().unwrap();
    assert_eq!(reads.len(), 3);
    for (index, read) in reads.iter().enumerate() {
        assert_eq!(read["index"], index);
        assert_eq!(
            read["hex"].as_str().unwrap().len() as u64,
            read["length"].as_u64().unwrap() * 2
        );
    }
    let single = ok(
        &mut host,
        "read_memory",
        json!({"memory_type": "ram", "address": COUNTER, "length": 8}),
    );
    assert_eq!(reads[0]["hex"], single["hex"]);
    for key in ["runtime_generation", "stop_epoch", "memory_mapping_epoch"] {
        assert!(!batch["boundary"][key].as_str().unwrap().is_empty());
    }
    assert_eq!(
        err_kind(
            &mut host,
            "read_memory_batch",
            json!({"ranges": [{"memory_type": "cpu", "address": 0x1f80_1040u32, "length": 4}]})
        ),
        "bad_params"
    );
    ok(&mut host, "resume", json!({}));
    assert_eq!(
        err_kind(
            &mut host,
            "read_memory_batch",
            json!({"ranges": [{"memory_type": "ram", "address": 0, "length": 4}]})
        ),
        "bad_state"
    );
}

// --- state, reset, video --------------------------------------------------------------

#[test]
fn save_and_load_state_round_trip_the_guest() {
    let dir = scratch_dir("state");
    let path = dir.join("checkpoint.psxstate");
    let mut host = host();
    ok(&mut host, "step", json!({"frames": 5}));
    let saved = ok(
        &mut host,
        "save_state",
        json!({"path": path.display().to_string()}),
    );
    assert_eq!(saved["format"], "psoxide-savestate-v1");
    assert!(saved["bytes"].as_u64().unwrap() > 0);
    let polls_at_save = saved["input_polls"].as_u64().unwrap();

    ok(&mut host, "step", json!({"frames": 9}));
    let later_counter = read_u32(&mut host, COUNTER);
    let later_hash = ok(&mut host, "psoxide_hashes", json!({}))["vram_fnv1a_64"].clone();

    let polls_before_load = ok(&mut host, "status", json!({}))["input_polls"]
        .as_u64()
        .unwrap();
    assert!(polls_before_load > polls_at_save);
    ok(&mut host, "set_input", json!({"buttons": []}));
    let loaded = ok(
        &mut host,
        "load_state",
        json!({"path": path.display().to_string()}),
    );
    assert_eq!(loaded["state"], "frozen");
    assert_eq!(
        loaded["frame"], 14,
        "the host frame counter stays monotonic"
    );
    assert_eq!(
        loaded["input_polls"], polls_before_load,
        "the host poll counter stays monotonic"
    );
    // PSoXide keeps the pad's poll counter out of save states, so the tape
    // clock (as after `launch --savestate`) restarts at zero.
    assert_eq!(loaded["guest_pad_polls"], 0);
    ok(&mut host, "step", json!({"frames": 9}));
    assert_eq!(
        read_u32(&mut host, COUNTER),
        later_counter,
        "the replay after load is identical"
    );
    assert_eq!(
        ok(&mut host, "psoxide_hashes", json!({}))["vram_fnv1a_64"],
        later_hash
    );

    assert_eq!(
        err_kind(
            &mut host,
            "load_state",
            json!({"path": dir.join("missing").display().to_string()})
        ),
        "bad_params"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn reset_reboots_and_keeps_the_persistent_override() {
    use emulator_core::button::START;
    let mut host = host();
    ok(&mut host, "set_input", json!({"buttons": ["start"]}));
    ok(&mut host, "step", json!({"frames": 4}));
    assert!(read_u32(&mut host, COUNTER) > 0);
    let reset = ok(&mut host, "reset", json!({}));
    assert_eq!(reset["reset_seq"], 1);
    assert_eq!(reset["boundary"], "pre_first_instruction");
    assert_eq!(
        read_u32(&mut host, COUNTER),
        0,
        "RAM is fresh after the reboot"
    );
    assert_eq!(
        ok(&mut host, "get_state", json!({}))["state"]["cpu.pc"],
        BASE
    );
    ok(&mut host, "step_input_polls", json!({"count": 1}));
    settle(&mut host);
    assert_eq!(wire_word(&mut host), !START);
}

#[test]
fn screenshots_are_png_with_provenance() {
    let mut host = host();
    // The synthetic guest never programs the GPU display: an explicit failure,
    // not a blank image.
    assert_eq!(err_kind(&mut host, "screenshot", json!({})), "bad_state");
    let shot = ok(&mut host, "screenshot", json!({"source": "vram"}));
    assert_eq!(shot["width"], 1024);
    assert_eq!(shot["height"], 512);
    use base64::Engine as _;
    let png = base64::engine::general_purpose::STANDARD
        .decode(shot["png_base64"].as_str().unwrap())
        .unwrap();
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert_eq!(shot["byte_len"], png.len());
    use sha2::Digest as _;
    let digest: String = sha2::Sha256::digest(&png)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(shot["sha256"], digest);
}

#[test]
fn run_state_follows_pause_resume_and_steps() {
    let mut host = host();
    assert_eq!(ok(&mut host, "status", json!({}))["state"], "frozen");
    assert_eq!(ok(&mut host, "resume", json!({}))["state"], "running");
    host.run_free_frame();
    host.run_free_frame();
    assert_eq!(ok(&mut host, "status", json!({}))["frame"], 2);
    let paused = ok(&mut host, "pause", json!({}));
    assert_eq!(paused["state"], "frozen");
    host.run_free_frame();
    assert_eq!(
        ok(&mut host, "status", json!({}))["frame"],
        2,
        "frozen means no free progress"
    );
    ok(&mut host, "resume", json!({}));
    let step = ok(&mut host, "step", json!({"frames": 1}));
    assert_eq!(step["was_running"], true);
    assert_eq!(step["state"], "frozen");
}

#[test]
fn rom_info_hashes_the_booted_image() {
    let mut host = host();
    let info = ok(&mut host, "get_rom_info", json!({}));
    use sha1::Digest as _;
    let expected: String = sha1::Sha1::digest(guest_exe())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(info["sha1"], expected);
    assert_eq!(info["system"], "psx");
    assert_eq!(info["media_type"], "exe");
}
