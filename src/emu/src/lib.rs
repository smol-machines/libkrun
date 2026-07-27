// Copyright 2026 The libkrun Authors. Licensed under Apache-2.0.
//
// Pure-Rust emulated riscv64 CPU core. No OS-specific dependencies: this
// crate must keep compiling for wasm32 targets.

pub mod cpu;
pub mod csr;
pub mod decode;
pub mod elf;
mod exec;
mod fpu;
pub mod mem;
pub mod mmu;
pub mod plic;
pub mod sbi;
pub mod trap;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub use cpu::Cpu;
pub use mem::GuestRam;
pub use trap::Exception;

/// Privilege modes, numbered as in the privileged spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PrivMode {
    U = 0,
    S = 1,
    M = 3,
}

/// Reasons `Cpu::run()` returns to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmExit {
    /// Load outside guest RAM. The cpu holds the target register and size;
    /// pc does not advance until `Cpu::complete_mmio_read()` supplies the data.
    MmioRead { addr: u64, size: u8 },
    /// Store outside guest RAM. pc has already advanced; the caller performs
    /// the bus write and calls `run()` again.
    MmioWrite { addr: u64, size: u8, data: u64 },
    /// `ecall` reported to the caller (SBI): only from S-mode with
    /// `sbi_mode` enabled. pc has advanced past the ecall.
    Ecall { from: PrivMode },
    /// `wfi` with no pending-and-enabled interrupt. pc has advanced.
    Wfi,
    /// A kick was observed.
    Interrupted,
    /// The instruction budget was exhausted.
    InstrLimit,
    /// A store hit the registered `tohost` address; `code` is the 64-bit
    /// value now in that RAM word (HTIF encoding).
    Shutdown { code: u64 },
}

/// Interrupt lines, valued by their mip/mie bit position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqLine {
    SSoft = 1,
    MSoft = 3,
    STimer = 5,
    MTimer = 7,
    SExt = 9,
    MExt = 11,
}

/// Source for the `time` CSR.
#[derive(Debug, Clone)]
pub enum Clock {
    /// time = instret >> shift. Fully reproducible; used by tests.
    Deterministic { shift: u32 },
    /// time is maintained by the VMM (shared guest timebase).
    External(Arc<AtomicU64>),
}

impl Clock {
    pub fn now(&self, instret: u64) -> u64 {
        match self {
            Clock::Deterministic { shift } => instret >> shift,
            Clock::External(t) => t.load(Ordering::Acquire),
        }
    }
}
