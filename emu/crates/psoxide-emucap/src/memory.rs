//! Memory regions the host exposes, and side-effect-free access to them.
//!
//! Reads never go through the bus's MMIO path: they come straight from device
//! storage (`Bus::ram`, `Bus::try_read8`, VRAM words), so observing memory can
//! never change guest state. Writes go through `Bus::write8_safe`, which keeps
//! the RAM page bookkeeping the CPU's block cache relies on.

use serde_json::{json, Value};

use crate::machine::Machine;

const RAM_SIZE: u64 = 2 * 1024 * 1024;
const SCRATCHPAD_BASE: u32 = 0x1f80_0000;
const SCRATCHPAD_SIZE: u64 = 1024;
const BIOS_BASE: u32 = 0x1fc0_0000;
const BIOS_SIZE: u64 = 512 * 1024;
const VRAM_SIZE: u64 = 1024 * 512 * 2;
const CPU_SPACE: u64 = 1 << 32;
/// Physical end of main RAM and its three mirrors.
const RAM_MIRROR_END: u32 = 0x0080_0000;

/// Batch limits advertised in `memory_batch_capability` (emucap's core caps).
pub const BATCH_MAX_RANGES: usize = 64;
/// Largest total byte count of one batch.
pub const BATCH_MAX_TOTAL_BYTES: u64 = 64 * 1024;

/// One addressable region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryType {
    /// 2 MiB main RAM, offset 0.
    Ram,
    /// 1 KiB data-cache scratchpad, offset 0.
    Scratchpad,
    /// 512 KiB BIOS ROM image (the HLE kernel's synthetic BIOS when no
    /// retail BIOS is used), read-only.
    Bios,
    /// 1 MiB VRAM as 1024x512 little-endian 16-bit words, row-major; read-only.
    Vram,
    /// The R3000A virtual address space (KUSEG/KSEG0/KSEG1), limited to RAM,
    /// scratchpad and BIOS.
    Cpu,
}

const ALL: [MemoryType; 5] = [
    MemoryType::Ram,
    MemoryType::Scratchpad,
    MemoryType::Bios,
    MemoryType::Vram,
    MemoryType::Cpu,
];

impl MemoryType {
    /// Wire name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Ram => "ram",
            Self::Scratchpad => "scratchpad",
            Self::Bios => "bios",
            Self::Vram => "vram",
            Self::Cpu => "cpu",
        }
    }

    /// Parse a wire name (case-insensitive).
    pub fn from_name(name: &str) -> Option<Self> {
        ALL.into_iter()
            .find(|kind| kind.name().eq_ignore_ascii_case(name))
    }

    fn size(self) -> u64 {
        match self {
            Self::Ram => RAM_SIZE,
            Self::Scratchpad => SCRATCHPAD_SIZE,
            Self::Bios => BIOS_SIZE,
            Self::Vram => VRAM_SIZE,
            Self::Cpu => CPU_SPACE,
        }
    }
}

/// All memory type names, in advertisement order.
pub fn type_names() -> Vec<&'static str> {
    ALL.iter().map(|kind| kind.name()).collect()
}

/// Exact region sizes for `hello.memory_regions`.
pub fn regions() -> Value {
    Value::Array(
        ALL.iter()
            .map(|kind| json!({"memory_type": kind.name(), "size": kind.size()}))
            .collect(),
    )
}

/// Side-effect-free windows a batch range must lie inside.
fn batch_windows() -> Vec<(MemoryType, u64, u64)> {
    let mut windows: Vec<(MemoryType, u64, u64)> = [
        MemoryType::Ram,
        MemoryType::Scratchpad,
        MemoryType::Bios,
        MemoryType::Vram,
    ]
    .iter()
    .map(|&kind| (kind, 0, kind.size()))
    .collect();
    for segment in [0x0000_0000u64, 0x8000_0000, 0xa000_0000] {
        windows.push((MemoryType::Cpu, segment, RAM_SIZE));
    }
    windows
}

/// `hello.memory_batch_capability`.
pub fn batch_capability() -> Value {
    json!({
        "max_ranges": BATCH_MAX_RANGES,
        "max_range_bytes": BATCH_MAX_TOTAL_BYTES,
        "max_total_bytes": BATCH_MAX_TOTAL_BYTES,
        "consistency": "frozen_boundary",
        "halt_kinds": ["frame_boundary", "instruction_boundary", "pre_first_instruction"],
        "windows": batch_windows()
            .into_iter()
            .map(|(kind, address, length)| json!({"memory_type": kind.name(), "address": address, "length": length}))
            .collect::<Vec<_>>(),
    })
}

/// Validate every batch range before any read (`MEM.BATCH.EXACT.1`).
pub fn admit_batch(ranges: &[(MemoryType, u64, u64)]) -> Result<(), String> {
    if ranges.is_empty() || ranges.len() > BATCH_MAX_RANGES {
        return Err(format!(
            "ranges must contain 1..={BATCH_MAX_RANGES} entries"
        ));
    }
    let windows = batch_windows();
    let mut total = 0u64;
    for (index, &(kind, address, length)) in ranges.iter().enumerate() {
        if length == 0 || length > BATCH_MAX_TOTAL_BYTES {
            return Err(format!(
                "range {index} length must be 1..={BATCH_MAX_TOTAL_BYTES}"
            ));
        }
        let end = address
            .checked_add(length)
            .ok_or_else(|| format!("range {index} overflows"))?;
        let inside = windows.iter().any(|&(w_kind, w_address, w_length)| {
            w_kind == kind && address >= w_address && end <= w_address + w_length
        });
        if !inside {
            return Err(format!(
                "range {index} ({} {address:#x}+{length}) is outside the advertised batch windows",
                kind.name()
            ));
        }
        total += length;
        if total > BATCH_MAX_TOTAL_BYTES {
            return Err(format!(
                "ranges exceed max_total_bytes {BATCH_MAX_TOTAL_BYTES}"
            ));
        }
    }
    Ok(())
}

fn check_bounds(kind: MemoryType, address: u64, length: u64) -> Result<(), String> {
    let end = address
        .checked_add(length)
        .ok_or_else(|| "address + length overflows".to_string())?;
    if end > kind.size() {
        return Err(format!(
            "{} range {address:#x}+{length:#x} exceeds the region size {:#x}",
            kind.name(),
            kind.size()
        ));
    }
    Ok(())
}

/// What a CPU virtual address resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CpuTarget {
    /// Main RAM (or a mirror) / BIOS, readable; RAM writable.
    Ram,
    Scratchpad,
    Bios,
}

fn classify_cpu(address: u32) -> Option<CpuTarget> {
    let (physical, kseg1) = match address {
        0x0000_0000..=0x1fff_ffff => (address, false),
        0x8000_0000..=0x9fff_ffff => (address - 0x8000_0000, false),
        0xa000_0000..=0xbfff_ffff => (address - 0xa000_0000, true),
        _ => return None,
    };
    if physical < RAM_MIRROR_END {
        Some(CpuTarget::Ram)
    } else if !kseg1
        && (SCRATCHPAD_BASE..SCRATCHPAD_BASE + SCRATCHPAD_SIZE as u32).contains(&physical)
    {
        // The scratchpad sits on the data cache: there is no uncached alias.
        Some(CpuTarget::Scratchpad)
    } else if (BIOS_BASE..BIOS_BASE + BIOS_SIZE as u32).contains(&physical) {
        Some(CpuTarget::Bios)
    } else {
        None
    }
}

fn cpu_byte_address(address: u64, offset: u64) -> u32 {
    (address + offset) as u32
}

/// Read `length` bytes. Fails without reading anything if any byte is outside
/// the region (or, for `cpu`, not backed by RAM/scratchpad/BIOS).
pub fn read(
    machine: &Machine,
    kind: MemoryType,
    address: u64,
    length: u64,
) -> Result<Vec<u8>, String> {
    check_bounds(kind, address, length)?;
    let bus = machine.bus();
    let start = address as usize;
    let end = start + length as usize;
    Ok(match kind {
        MemoryType::Ram => bus.ram()[start..end].to_vec(),
        MemoryType::Scratchpad | MemoryType::Bios => {
            let base = if kind == MemoryType::Scratchpad {
                SCRATCHPAD_BASE
            } else {
                BIOS_BASE
            };
            (start..end)
                .map(|offset| {
                    bus.try_read8(base + offset as u32)
                        .expect("device storage is mapped")
                })
                .collect()
        }
        MemoryType::Vram => {
            let words = bus.gpu.vram.words();
            (start..end)
                .map(|offset| {
                    let word = words[offset / 2];
                    if offset % 2 == 0 {
                        word as u8
                    } else {
                        (word >> 8) as u8
                    }
                })
                .collect()
        }
        MemoryType::Cpu => {
            for offset in 0..length {
                let byte_address = cpu_byte_address(address, offset);
                if classify_cpu(byte_address).is_none() {
                    return Err(format!(
                        "cpu address {byte_address:#010x} is not RAM, scratchpad or BIOS; I/O registers are not readable without side effects"
                    ));
                }
            }
            (0..length)
                .map(|offset| {
                    bus.try_read8(cpu_byte_address(address, offset))
                        .expect("classified addresses are mapped")
                })
                .collect()
        }
    })
}

/// Write `bytes`. Validates the whole range first, so a refused write changes
/// nothing (`CORE.REJECT.1`).
pub fn write(
    machine: &mut Machine,
    kind: MemoryType,
    address: u64,
    bytes: &[u8],
) -> Result<(), String> {
    let length = bytes.len() as u64;
    check_bounds(kind, address, length).map_err(|e| format!("bad range: {e}"))?;
    let targets: Vec<u32> = match kind {
        MemoryType::Ram => (0..length)
            .map(|offset| (address + offset) as u32)
            .collect(),
        MemoryType::Scratchpad => (0..length)
            .map(|offset| SCRATCHPAD_BASE + (address + offset) as u32)
            .collect(),
        MemoryType::Bios | MemoryType::Vram => {
            return Err(format!("{} is read-only in this host", kind.name()));
        }
        MemoryType::Cpu => {
            let mut targets = Vec::with_capacity(bytes.len());
            for offset in 0..length {
                let byte_address = cpu_byte_address(address, offset);
                match classify_cpu(byte_address) {
                    Some(CpuTarget::Ram | CpuTarget::Scratchpad) => targets.push(byte_address),
                    Some(CpuTarget::Bios) => {
                        return Err(format!(
                            "cpu address {byte_address:#010x} is BIOS ROM (read-only)"
                        ));
                    }
                    None => {
                        return Err(format!(
                            "cpu address {byte_address:#010x} is not RAM or scratchpad; I/O writes are not exposed"
                        ));
                    }
                }
            }
            targets
        }
    };
    let bus = machine.bus_mut();
    for (&target, &byte) in targets.iter().zip(bytes) {
        let written = bus.write8_safe(target, byte);
        debug_assert!(written, "validated address {target:#x} must be writable");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_classification_follows_the_segment_map() {
        assert_eq!(classify_cpu(0x0000_0010), Some(CpuTarget::Ram));
        assert_eq!(classify_cpu(0x8001_0000), Some(CpuTarget::Ram));
        assert_eq!(classify_cpu(0xa07f_ffff), Some(CpuTarget::Ram));
        assert_eq!(classify_cpu(0x1f80_0000), Some(CpuTarget::Scratchpad));
        assert_eq!(classify_cpu(0x9f80_03ff), Some(CpuTarget::Scratchpad));
        assert_eq!(
            classify_cpu(0xbf80_0000),
            None,
            "no uncached scratchpad alias"
        );
        assert_eq!(classify_cpu(0xbfc0_0000), Some(CpuTarget::Bios));
        assert_eq!(classify_cpu(0x1f80_1810), None, "GPU port is I/O");
        assert_eq!(classify_cpu(0xfffe_0130), None, "KSEG2 cache control");
    }

    #[test]
    fn batch_admission_rejects_before_reading() {
        assert!(
            admit_batch(&[(MemoryType::Ram, 0, 16), (MemoryType::Cpu, 0x8000_0000, 16)]).is_ok()
        );
        assert!(admit_batch(&[]).is_err());
        assert!(admit_batch(&[(MemoryType::Ram, RAM_SIZE - 1, 2)]).is_err());
        assert!(admit_batch(&[(MemoryType::Cpu, 0x1f80_1800, 4)]).is_err());
        let too_much = vec![(MemoryType::Ram, 0, 2048); 33];
        assert!(admit_batch(&too_much).is_err());
    }
}
