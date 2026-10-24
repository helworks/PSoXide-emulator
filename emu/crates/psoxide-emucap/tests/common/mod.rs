//! Hand-assembled PSX-EXE guests shared by the integration tests.
//!
//! Both guests poll the port-1 pad over SIO0 exactly the way a game's pad
//! driver does and store the wire button word and a poll counter in RAM. The
//! drawing guest also folds every polled word into a running hash and writes
//! it into VRAM at the top-left of the display, so the displayed picture
//! depends on the whole input history.

// Each test crate uses a different subset.
#![allow(dead_code)]

pub const BASE: u32 = 0x8001_0000;
/// Guest poll counter (RAM offset).
pub const COUNTER: u64 = 0x2_0000;
/// Guest copy of the last polled 16-bit button word, as on the wire (active-low).
pub const WIRE: u64 = 0x2_0004;
/// Drawing guest: running hash of every polled wire word (RAM offset).
pub const INPUT_HASH: u64 = 0x2_0008;

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
const S1: u32 = 17;
const S2: u32 = 18;
const RA: u32 = 31;

fn i(op: u32, rs: u32, rt: u32, imm: u32) -> u32 {
    (op << 26) | (rs << 21) | (rt << 16) | (imm & 0xffff)
}
fn r(rs: u32, rt: u32, rd: u32, sa: u32, funct: u32) -> u32 {
    (rs << 21) | (rt << 16) | (rd << 11) | (sa << 6) | funct
}
pub fn at(index: u32) -> u32 {
    BASE + index * 4
}
fn branch(op: u32, rs: u32, rt: u32, from: u32, to: u32) -> u32 {
    let offset = (at(to) as i64 - (at(from) as i64 + 4)) / 4;
    i(op, rs, rt, offset as u32)
}
pub fn jump(op: u32, to: u32) -> u32 {
    (op << 26) | ((at(to) >> 2) & 0x03ff_ffff)
}

/// The pad-polling guest. Instruction indices are referenced by the branches.
pub fn guest_program() -> Vec<u32> {
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

/// The pad-polling guest plus a display: it configures a 256x240 display,
/// keeps `hash = hash * 31 + wire` over every polled word, stores it at
/// [`INPUT_HASH`] and uploads it as the two top-left display pixels, so one
/// changed poll changes every later displayed frame.
pub fn drawing_guest_program() -> Vec<u32> {
    let main = 11;
    let delay = 47;
    let xchg = 52;
    let wait = 53;
    let program = vec![
        /* 0 */ i(0x0f, 0, T2, 0x1f80), // lui t2, 0x1f80
        /* 1 */ i(0x0d, T2, T2, 0x1040), // ori t2, t2, 0x1040 (SIO0)
        /* 2 */ i(0x0f, 0, T0, 0x8002), // lui t0, 0x8002 (counter base)
        /* 3 */ i(0x0f, 0, S2, 0x1f80), // lui s2, 0x1f80
        /* 4 */ i(0x0d, S2, S2, 0x1810), // ori s2, s2, 0x1810 (GP0; GP1 at +4)
        /* 5 */ i(0x0f, 0, T3, 0x0800), // lui t3, 0x0800: GP1 display mode 256x240
        /* 6 */ i(0x2b, S2, T3, 4), // sw t3, 4(s2)
        /* 7 */ i(0x0f, 0, T3, 0x0704), // lui t3, 0x0704: GP1 vertical range
        /* 8 */ i(0x0d, T3, T3, 0x0010), // ori t3, t3, 0x10: lines 16..256
        /* 9 */ i(0x2b, S2, T3, 4), // sw t3, 4(s2)
        /* 10 */ i(0x0d, ZERO, S1, 0), // ori s1, zero, 0 (input hash)
        /* 11 main */ i(0x0d, ZERO, T3, 2), // ori t3, zero, 2
        /* 12 */ i(0x29, T2, T3, 0xa), // sh t3, 0xa(t2): select port 1
        /* 13 */ i(0x0d, ZERO, T4, 0x01),
        /* 14 */ jump(0x03, xchg),
        /* 15 */ 0,
        /* 16 */ i(0x0d, ZERO, T4, 0x42),
        /* 17 */ jump(0x03, xchg),
        /* 18 */ 0,
        /* 19 */ i(0x0d, ZERO, T4, 0x00),
        /* 20 */ jump(0x03, xchg), // id high
        /* 21 */ 0,
        /* 22 */ jump(0x03, xchg), // buttons low
        /* 23 */ 0,
        /* 24 */ r(T6, ZERO, S0, 0, 0x25), // or s0, t6, zero
        /* 25 */ jump(0x03, xchg), // buttons high
        /* 26 */ 0,
        /* 27 */ i(0x29, T2, ZERO, 0xa), // sh zero, 0xa(t2): deselect
        /* 28 */ r(0, T6, T6, 8, 0x00), // sll t6, t6, 8
        /* 29 */ r(S0, T6, S0, 0, 0x25), // or s0, s0, t6
        /* 30 */ i(0x2b, T0, S0, 4), // sw s0, 4(t0)
        /* 31 */ i(0x23, T0, T1, 0), // lw t1, 0(t0)
        /* 32 */ 0,
        /* 33 */ i(0x09, T1, T1, 1), // addiu t1, t1, 1
        /* 34 */ i(0x2b, T0, T1, 0), // sw t1, 0(t0)
        /* 35 */ r(0, S1, T3, 5, 0x00), // sll t3, s1, 5
        /* 36 */ r(T3, S1, S1, 0, 0x23), // subu s1, t3, s1 (s1 * 31)
        /* 37 */ r(S1, S0, S1, 0, 0x21), // addu s1, s1, s0
        /* 38 */ i(0x2b, T0, S1, 8), // sw s1, 8(t0)
        /* 39 */ i(0x0f, 0, T3, 0xa000), // lui t3, 0xa000: GP0 copy CPU -> VRAM
        /* 40 */ i(0x2b, S2, T3, 0), // sw t3, 0(s2)
        /* 41 */ i(0x2b, S2, ZERO, 0), // sw zero, 0(s2): at (0, 0)
        /* 42 */ i(0x0f, 0, T3, 0x0001), // lui t3, 1
        /* 43 */ i(0x0d, T3, T3, 0x0002), // ori t3, t3, 2: 2x1 pixels
        /* 44 */ i(0x2b, S2, T3, 0), // sw t3, 0(s2)
        /* 45 */ i(0x2b, S2, S1, 0), // sw s1, 0(s2): the two pixels
        /* 46 */ i(0x0f, 0, T7, 0x0001), // lui t7, 1 (delay loop count)
        /* 47 delay */ i(0x09, T7, T7, 0xffff), // addiu t7, t7, -1
        /* 48 */ branch(0x05, T7, ZERO, 48, delay), // bne t7, zero, delay
        /* 49 */ 0,
        /* 50 */ jump(0x02, main), // j main
        /* 51 */ 0,
        /* 52 xchg */ i(0x28, T2, T4, 0), // sb t4, 0(t2)
        /* 53 wait */ i(0x25, T2, T5, 4), // lhu t5, 4(t2): JOY_STAT
        /* 54 */ 0,
        /* 55 */ i(0x0c, T5, T5, 2), // andi t5, t5, 2: RX not empty
        /* 56 */ branch(0x04, T5, ZERO, 56, wait), // beq t5, zero, wait
        /* 57 */ 0,
        /* 58 */ i(0x24, T2, T6, 0), // lbu t6, 0(t2)
        /* 59 */ r(RA, 0, 0, 0, 0x08), // jr ra
        /* 60 */ 0,
    ];
    assert_eq!(program.len(), 61);
    program
}

/// Wrap `program` in a `PS-X EXE` header loading and starting at [`BASE`].
pub fn exe_from_program(program: &[u32]) -> Vec<u8> {
    let payload: Vec<u8> = program.iter().flat_map(|w| w.to_le_bytes()).collect();
    let mut exe = vec![0u8; 0x800];
    exe[..8].copy_from_slice(b"PS-X EXE");
    exe[0x10..0x14].copy_from_slice(&BASE.to_le_bytes());
    exe[0x18..0x1c].copy_from_slice(&BASE.to_le_bytes());
    exe[0x1c..0x20].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    exe[0x30..0x34].copy_from_slice(&0x801f_fff0u32.to_le_bytes());
    exe.extend_from_slice(&payload);
    exe
}

/// The pad-polling guest as an EXE.
pub fn guest_exe() -> Vec<u8> {
    exe_from_program(&guest_program())
}

/// The drawing guest as an EXE.
pub fn drawing_guest_exe() -> Vec<u8> {
    exe_from_program(&drawing_guest_program())
}
