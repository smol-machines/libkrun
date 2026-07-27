// Copyright 2026 The libkrun Authors. Licensed under Apache-2.0.

//! The SBI (Supervisor Binary Interface) firmware layer, shared by every
//! embedder of the emulated CPU.
//!
//! With this backend the embedder plays the role of SBI firmware, exactly like
//! KVM-riscv: the guest kernel runs in S-mode and its `ecall`s surface as
//! `VmExit::Ecall { from: PrivMode::S }`.
//!
//! Calling convention (SBI v2.0, binary encoding): extension id in `a7`,
//! function id in `a6`, arguments in `a0..a5`; returns error code in `a0` and
//! value in `a1`.
//!
//! The split is spec vs. effects, not portable vs. native. Everything the
//! specification pins down — constants, request marshalling, hart-mask
//! decoding, and the [`dispatch`] tree itself — lives here, so there is one
//! implementation of SBI to keep correct. What an embedder supplies is the
//! [`SbiHost`] impl: the effectful side (timers, IPIs, remote fences, HSM hart
//! lifecycle, console, shutdown), which is the only part that genuinely
//! differs between a multi-threaded vCPU run loop and a single-threaded
//! machine.

use crate::Cpu;

/// riscv64 integer register indices used by the SBI calling convention.
const REG_A0: usize = 10;
const REG_A1: usize = 11;
const REG_A6: usize = 16;
const REG_A7: usize = 17;

/// SBI standard error codes (returned in `a0`).
pub const SBI_SUCCESS: i64 = 0;
pub const SBI_ERR_NOT_SUPPORTED: i64 = -2;
pub const SBI_ERR_INVALID_PARAM: i64 = -3;
pub const SBI_ERR_INVALID_ADDRESS: i64 = -5;
pub const SBI_ERR_ALREADY_AVAILABLE: i64 = -6;

/// Extension ids.
pub const EID_BASE: u64 = 0x10;
pub const EID_TIME: u64 = 0x5449_4D45; // "TIME"
pub const EID_IPI: u64 = 0x0073_5049; // "sPI"
pub const EID_RFENCE: u64 = 0x5246_4E43; // "RFNC"
pub const EID_HSM: u64 = 0x0048_534D; // "HSM"
pub const EID_SRST: u64 = 0x5352_5354; // "SRST"
pub const EID_DBCN: u64 = 0x4442_434E; // "DBCN"

/// SBI v0.1 legacy console calls. Only served when the host opts in via
/// [`SbiHost::legacy_console`]; `console_putchar` is the cheapest possible
/// early console for embedders that have nowhere else to put boot output.
pub const EID_LEGACY_PUTCHAR: u64 = 0x01;
pub const EID_LEGACY_GETCHAR: u64 = 0x02;

/// Base extension function ids.
pub const BASE_GET_SPEC_VERSION: u64 = 0;
pub const BASE_GET_IMPL_ID: u64 = 1;
pub const BASE_GET_IMPL_VERSION: u64 = 2;
pub const BASE_PROBE_EXTENSION: u64 = 3;
pub const BASE_GET_MVENDORID: u64 = 4;
pub const BASE_GET_MARCHID: u64 = 5;
pub const BASE_GET_MIMPID: u64 = 6;

/// TIME extension function ids.
pub const TIME_SET_TIMER: u64 = 0;

/// IPI extension function ids.
pub const IPI_SEND_IPI: u64 = 0;

/// RFENCE extension function ids. The three supervisor fences differ only in
/// the range/asid arguments a full local flush ignores.
pub const RFENCE_FENCE_I: u64 = 0;
pub const RFENCE_SFENCE_VMA: u64 = 1;
pub const RFENCE_SFENCE_VMA_ASID: u64 = 2;

/// HSM extension function ids.
pub const HSM_HART_START: u64 = 0;
pub const HSM_HART_STOP: u64 = 1;
pub const HSM_HART_GET_STATUS: u64 = 2;

/// SRST extension function ids, and the reset types of `system_reset`.
pub const SRST_SYSTEM_RESET: u64 = 0;
pub const SRST_TYPE_SHUTDOWN: u64 = 0;
pub const SRST_TYPE_COLD_REBOOT: u64 = 1;
pub const SRST_TYPE_WARM_REBOOT: u64 = 2;

/// DBCN extension function ids.
pub const DBCN_CONSOLE_WRITE: u64 = 0;
pub const DBCN_CONSOLE_READ: u64 = 1;
pub const DBCN_CONSOLE_WRITE_BYTE: u64 = 2;

/// SBI spec version implemented: v2.0, encoded as `minor | (major << 24)`.
pub const SPEC_VERSION: u64 = 2 << 24;
/// Implementation id: 3 = "KVM" — we transplant the KVM-riscv SBI model
/// (guest enters S-mode directly, the hypervisor is the firmware), and the
/// guest-visible behavior intentionally mirrors it.
pub const IMPL_ID: u64 = 3;
pub const IMPL_VERSION: u64 = 0;

/// HSM hart states (`hart_get_status` return values).
pub const HSM_STATE_STARTED: u64 = 0;
pub const HSM_STATE_STOPPED: u64 = 1;
pub const HSM_STATE_START_PENDING: u64 = 2;
pub const HSM_STATE_STOP_PENDING: u64 = 3;

/// A decoded SBI call: `a7`/`a6`/`a0..a5` at the `ecall`.
#[derive(Debug, Clone, Copy)]
pub struct SbiRequest {
    pub eid: u64,
    pub fid: u64,
    pub args: [u64; 6],
}

impl SbiRequest {
    /// Read the calling convention out of a hart's registers.
    pub fn from_cpu(cpu: &Cpu) -> Self {
        SbiRequest {
            eid: cpu.read_reg(REG_A7),
            fid: cpu.read_reg(REG_A6),
            args: [
                cpu.read_reg(REG_A0),
                cpu.read_reg(REG_A1),
                cpu.read_reg(12),
                cpu.read_reg(13),
                cpu.read_reg(14),
                cpu.read_reg(15),
            ],
        }
    }
}

/// Write the standard SBI return pair back into the calling hart: error code
/// in `a0`, value in `a1`.
pub fn set_return(cpu: &mut Cpu, error: i64, value: u64) {
    cpu.set_reg(REG_A0, error as u64);
    cpu.set_reg(REG_A1, value);
}

/// Decode an SBI `hart_mask`/`hart_mask_base` pair into target hart ids.
///
/// Per the spec, `hart_mask_base == -1` selects all harts (and `hart_mask`
/// is ignored); otherwise bit `i` of `hart_mask` selects hart
/// `hart_mask_base + i`. Selecting a hart outside the machine is an
/// `SBI_ERR_INVALID_PARAM`.
pub fn hart_mask_targets(
    hart_mask: u64,
    hart_mask_base: u64,
    nharts: u32,
) -> Result<Vec<u32>, i64> {
    if hart_mask_base == u64::MAX {
        return Ok((0..nharts).collect());
    }
    let mut targets = Vec::new();
    for bit in 0..64u64 {
        if hart_mask & (1 << bit) == 0 {
            continue;
        }
        let hart = hart_mask_base
            .checked_add(bit)
            .ok_or(SBI_ERR_INVALID_PARAM)?;
        if hart >= u64::from(nharts) {
            return Err(SBI_ERR_INVALID_PARAM);
        }
        targets.push(hart as u32);
    }
    Ok(targets)
}

/// Per-call cap on DBCN console writes; the spec allows a partial write, with
/// the number of bytes actually written returned in `a1`.
const DBCN_WRITE_CAP: usize = 4096;

/// The standard SBI return pair: error in `a0`, value in `a1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SbiRet {
    pub error: i64,
    pub value: u64,
}

impl SbiRet {
    pub fn ok(value: u64) -> Self {
        SbiRet {
            error: SBI_SUCCESS,
            value,
        }
    }

    pub fn err(error: i64) -> Self {
        SbiRet { error, value: 0 }
    }
}

/// What the caller must do after a dispatched call.
#[derive(Debug, PartialEq, Eq)]
pub enum SbiAction {
    /// Write `a0`/`a1` from the [`SbiRet`] (see [`set_return`]) and continue.
    Return(SbiRet),
    /// SRST system_reset: funnel into the shutdown path. This call does not
    /// return to the guest.
    Shutdown,
    /// HSM hart_stop: park the calling hart. Does not return on success.
    StopHart,
}

/// Host-side effects [`dispatch`] needs. Everything the SBI spec pins down is
/// handled by the dispatcher; an implementation of this trait supplies only
/// the parts that touch embedder-owned state.
pub trait SbiHost {
    /// Number of harts in the machine.
    fn nharts(&self) -> u32;
    /// TIME set_timer: program the calling hart's next timer event at
    /// absolute guest time `deadline` (ticks); `u64::MAX` cancels. The
    /// implementation must clear any pending STimer interrupt.
    fn set_timer(&mut self, deadline: u64);
    /// IPI: make SSIP pending on the target hart.
    fn send_ipi(&mut self, hart: u32);
    /// RFENCE: execute a remote fence on every target hart and do not return
    /// until each has completed it (correctness-over-speed: any of fence.i /
    /// sfence.vma / sfence.vma+asid is served as a full local flush).
    fn remote_fence(&mut self, targets: &[u32]);
    /// HSM hart_start: release `hart` at `pc` with `a1 = opaque`. `hart` is
    /// already known to be in range. Errors with an SBI error code (already
    /// started / invalid address).
    fn hart_start(&mut self, hart: u32, pc: u64, opaque: u64) -> Result<(), i64>;
    /// HSM hart_get_status for `hart` (one of the `HSM_STATE_*` values).
    /// `hart` is already known to be in range.
    fn hart_status(&self, hart: u32) -> u64;
    /// DBCN (and legacy putchar): sink console bytes.
    fn console_write(&mut self, bytes: &[u8]);
    /// Copy guest-physical memory into `buf`; false if out of bounds.
    fn guest_read(&self, gpa: u64, buf: &mut [u8]) -> bool;
    /// Whether to serve the SBI v0.1 legacy console calls. Off by default:
    /// probing them is how a guest decides to use them, so an embedder with a
    /// real console should not advertise them.
    fn legacy_console(&self) -> bool {
        false
    }
}

/// Dispatch one SBI call against `host`.
pub fn dispatch<H: SbiHost>(host: &mut H, call: &SbiRequest) -> SbiAction {
    match call.eid {
        EID_BASE => SbiAction::Return(base(host, call)),
        EID_TIME => SbiAction::Return(match call.fid {
            TIME_SET_TIMER => {
                host.set_timer(call.args[0]);
                SbiRet::ok(0)
            }
            _ => SbiRet::err(SBI_ERR_NOT_SUPPORTED),
        }),
        EID_IPI => SbiAction::Return(match call.fid {
            IPI_SEND_IPI => match hart_mask_targets(call.args[0], call.args[1], host.nharts()) {
                Ok(targets) => {
                    for hart in targets {
                        host.send_ipi(hart);
                    }
                    SbiRet::ok(0)
                }
                Err(e) => SbiRet::err(e),
            },
            _ => SbiRet::err(SBI_ERR_NOT_SUPPORTED),
        }),
        EID_RFENCE => SbiAction::Return(match call.fid {
            // remote_fence_i / remote_sfence_vma / remote_sfence_vma_asid:
            // all served as full remote flushes (see SbiHost::remote_fence);
            // the range/asid arguments (a2..a4) are accepted and ignored.
            RFENCE_FENCE_I..=RFENCE_SFENCE_VMA_ASID => {
                match hart_mask_targets(call.args[0], call.args[1], host.nharts()) {
                    Ok(targets) => {
                        host.remote_fence(&targets);
                        SbiRet::ok(0)
                    }
                    Err(e) => SbiRet::err(e),
                }
            }
            // Hypervisor (hfence) variants: no H extension.
            _ => SbiRet::err(SBI_ERR_NOT_SUPPORTED),
        }),
        EID_HSM => match call.fid {
            HSM_HART_START => SbiAction::Return(match in_range(host, call.args[0]) {
                Some(hart) => match host.hart_start(hart, call.args[1], call.args[2]) {
                    Ok(()) => SbiRet::ok(0),
                    Err(e) => SbiRet::err(e),
                },
                None => SbiRet::err(SBI_ERR_INVALID_PARAM),
            }),
            // Does not return on success.
            HSM_HART_STOP => SbiAction::StopHart,
            HSM_HART_GET_STATUS => SbiAction::Return(match in_range(host, call.args[0]) {
                Some(hart) => SbiRet::ok(host.hart_status(hart)),
                None => SbiRet::err(SBI_ERR_INVALID_PARAM),
            }),
            // hart_suspend and friends: not supported (the guest falls back
            // to plain WFI idling).
            _ => SbiAction::Return(SbiRet::err(SBI_ERR_NOT_SUPPORTED)),
        },
        EID_SRST => match call.fid {
            SRST_SYSTEM_RESET => {
                // system_reset(reset_type, reset_reason). Shutdown, cold and
                // warm reboot — with "reboot=k panic=-1" on the cmdline they
                // all funnel into the same shutdown.
                match call.args[0] {
                    SRST_TYPE_SHUTDOWN..=SRST_TYPE_WARM_REBOOT => SbiAction::Shutdown,
                    _ => SbiAction::Return(SbiRet::err(SBI_ERR_NOT_SUPPORTED)),
                }
            }
            _ => SbiAction::Return(SbiRet::err(SBI_ERR_NOT_SUPPORTED)),
        },
        EID_DBCN => SbiAction::Return(match call.fid {
            // console_write(num_bytes, base_addr_lo, base_addr_hi). The
            // address is guest-physical (Linux passes `__pa(buf)`).
            DBCN_CONSOLE_WRITE => {
                let num = call.args[0];
                let lo = call.args[1];
                let hi = call.args[2];
                if hi != 0 {
                    // RV64: the physical address is entirely in the lo half.
                    SbiRet::err(SBI_ERR_INVALID_PARAM)
                } else {
                    let len = usize::try_from(num)
                        .unwrap_or(usize::MAX)
                        .min(DBCN_WRITE_CAP);
                    let mut buf = vec![0u8; len];
                    if host.guest_read(lo, &mut buf) {
                        host.console_write(&buf);
                        SbiRet::ok(len as u64)
                    } else {
                        SbiRet::err(SBI_ERR_INVALID_ADDRESS)
                    }
                }
            }
            // No input source; report zero bytes read.
            DBCN_CONSOLE_READ => SbiRet::ok(0),
            DBCN_CONSOLE_WRITE_BYTE => {
                host.console_write(&[call.args[0] as u8]);
                SbiRet::ok(0)
            }
            _ => SbiRet::err(SBI_ERR_NOT_SUPPORTED),
        }),
        EID_LEGACY_PUTCHAR if host.legacy_console() => {
            host.console_write(&[call.args[0] as u8]);
            SbiAction::Return(SbiRet::ok(0))
        }
        // Legacy getchar returns the character in a0; -1 means "none".
        EID_LEGACY_GETCHAR if host.legacy_console() => SbiAction::Return(SbiRet {
            error: -1,
            value: 0,
        }),
        _ => SbiAction::Return(SbiRet::err(SBI_ERR_NOT_SUPPORTED)),
    }
}

/// `hart` as a valid hart id for `host`, or `None` if out of range.
fn in_range<H: SbiHost>(host: &H, hart: u64) -> Option<u32> {
    (hart < u64::from(host.nharts())).then_some(hart as u32)
}

fn base<H: SbiHost>(host: &H, call: &SbiRequest) -> SbiRet {
    match call.fid {
        BASE_GET_SPEC_VERSION => SbiRet::ok(SPEC_VERSION),
        BASE_GET_IMPL_ID => SbiRet::ok(IMPL_ID),
        BASE_GET_IMPL_VERSION => SbiRet::ok(IMPL_VERSION),
        BASE_PROBE_EXTENSION => SbiRet::ok(u64::from(match call.args[0] {
            EID_BASE | EID_TIME | EID_IPI | EID_RFENCE | EID_HSM | EID_SRST | EID_DBCN => true,
            EID_LEGACY_PUTCHAR | EID_LEGACY_GETCHAR => host.legacy_console(),
            _ => false,
        })),
        // No M-mode below the guest: mvendorid/marchid/mimpid all read 0.
        BASE_GET_MVENDORID | BASE_GET_MARCHID | BASE_GET_MIMPID => SbiRet::ok(0),
        _ => SbiRet::err(SBI_ERR_NOT_SUPPORTED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Clock, GuestRam};

    fn cpu() -> Cpu {
        Cpu::new(
            0,
            GuestRam::new_owned(0, 4096),
            Clock::Deterministic { shift: 0 },
        )
    }

    #[test]
    fn request_marshalling() {
        let mut cpu = cpu();
        cpu.set_reg(REG_A7, EID_HSM);
        cpu.set_reg(REG_A6, HSM_HART_START);
        for (i, r) in [REG_A0, REG_A1, 12, 13, 14, 15].into_iter().enumerate() {
            cpu.set_reg(r, 0x100 + i as u64);
        }
        let req = SbiRequest::from_cpu(&cpu);
        assert_eq!(req.eid, EID_HSM);
        assert_eq!(req.fid, HSM_HART_START);
        assert_eq!(req.args, [0x100, 0x101, 0x102, 0x103, 0x104, 0x105]);

        // The return pair lands in a0/a1, error sign-extended.
        set_return(&mut cpu, SBI_ERR_INVALID_PARAM, 42);
        assert_eq!(cpu.read_reg(REG_A0), SBI_ERR_INVALID_PARAM as u64);
        assert_eq!(cpu.read_reg(REG_A1), 42);
        set_return(&mut cpu, SBI_SUCCESS, 0);
        assert_eq!(cpu.read_reg(REG_A0), 0);
        assert_eq!(cpu.read_reg(REG_A1), 0);
    }

    #[test]
    fn hart_mask_decoding() {
        // base = -1: all harts, mask ignored.
        assert_eq!(hart_mask_targets(0, u64::MAX, 4).unwrap(), vec![0, 1, 2, 3]);
        // bit i selects base + i.
        assert_eq!(hart_mask_targets(0b101, 1, 4).unwrap(), vec![1, 3]);
        assert_eq!(hart_mask_targets(0b1, 0, 4).unwrap(), vec![0]);
        // empty mask selects nothing.
        assert_eq!(hart_mask_targets(0, 0, 4).unwrap(), Vec::<u32>::new());
        // out-of-range hart.
        assert_eq!(
            hart_mask_targets(0b1, 4, 4).unwrap_err(),
            SBI_ERR_INVALID_PARAM
        );
        // base + bit overflow.
        assert_eq!(
            hart_mask_targets(1 << 63, u64::MAX - 1, 4).unwrap_err(),
            SBI_ERR_INVALID_PARAM
        );
    }

    /// Records every effect so the dispatch tree can be tested without a
    /// machine behind it.
    #[derive(Default)]
    struct MockHost {
        nharts: u32,
        legacy: bool,
        timer: Option<u64>,
        ipis: Vec<u32>,
        fences: Vec<Vec<u32>>,
        started: Vec<(u32, u64, u64)>,
        console: Vec<u8>,
        guest_mem: Vec<u8>, // guest memory at gpa 0x1000
    }

    impl SbiHost for MockHost {
        fn nharts(&self) -> u32 {
            self.nharts
        }
        fn set_timer(&mut self, deadline: u64) {
            self.timer = Some(deadline);
        }
        fn send_ipi(&mut self, hart: u32) {
            self.ipis.push(hart);
        }
        fn remote_fence(&mut self, targets: &[u32]) {
            self.fences.push(targets.to_vec());
        }
        fn hart_start(&mut self, hart: u32, pc: u64, opaque: u64) -> Result<(), i64> {
            if self.started.iter().any(|&(h, _, _)| h == hart) {
                return Err(SBI_ERR_ALREADY_AVAILABLE);
            }
            self.started.push((hart, pc, opaque));
            Ok(())
        }
        fn hart_status(&self, hart: u32) -> u64 {
            if self.started.iter().any(|&(h, _, _)| h == hart) {
                HSM_STATE_STARTED
            } else {
                HSM_STATE_STOPPED
            }
        }
        fn console_write(&mut self, bytes: &[u8]) {
            self.console.extend_from_slice(bytes);
        }
        fn guest_read(&self, gpa: u64, buf: &mut [u8]) -> bool {
            let Some(off) = gpa.checked_sub(0x1000) else {
                return false;
            };
            let off = off as usize;
            if off + buf.len() > self.guest_mem.len() {
                return false;
            }
            buf.copy_from_slice(&self.guest_mem[off..off + buf.len()]);
            true
        }
        fn legacy_console(&self) -> bool {
            self.legacy
        }
    }

    fn call(eid: u64, fid: u64, args: [u64; 6]) -> SbiRequest {
        SbiRequest { eid, fid, args }
    }

    fn host() -> MockHost {
        MockHost {
            nharts: 4,
            ..Default::default()
        }
    }

    #[test]
    fn base_extension() {
        let mut h = host();
        assert_eq!(
            dispatch(&mut h, &call(EID_BASE, 0, [0; 6])),
            SbiAction::Return(SbiRet::ok(2 << 24))
        );
        assert_eq!(
            dispatch(&mut h, &call(EID_BASE, 1, [0; 6])),
            SbiAction::Return(SbiRet::ok(IMPL_ID))
        );
        // probe: supported extensions answer 1, others 0.
        for eid in [EID_TIME, EID_IPI, EID_RFENCE, EID_HSM, EID_SRST, EID_DBCN] {
            assert_eq!(
                dispatch(&mut h, &call(EID_BASE, 3, [eid, 0, 0, 0, 0, 0])),
                SbiAction::Return(SbiRet::ok(1))
            );
        }
        assert_eq!(
            dispatch(&mut h, &call(EID_BASE, 3, [0xdead, 0, 0, 0, 0, 0])),
            SbiAction::Return(SbiRet::ok(0))
        );
        // mvendorid/marchid/mimpid are 0.
        for fid in 4..=6 {
            assert_eq!(
                dispatch(&mut h, &call(EID_BASE, fid, [0; 6])),
                SbiAction::Return(SbiRet::ok(0))
            );
        }
    }

    #[test]
    fn unknown_extension_and_fid() {
        let mut h = host();
        assert_eq!(
            dispatch(&mut h, &call(0x0abcdef0, 0, [0; 6])),
            SbiAction::Return(SbiRet::err(SBI_ERR_NOT_SUPPORTED))
        );
        assert_eq!(
            dispatch(&mut h, &call(EID_TIME, 7, [0; 6])),
            SbiAction::Return(SbiRet::err(SBI_ERR_NOT_SUPPORTED))
        );
    }

    #[test]
    fn time_set_timer() {
        let mut h = host();
        assert_eq!(
            dispatch(&mut h, &call(EID_TIME, 0, [12345, 0, 0, 0, 0, 0])),
            SbiAction::Return(SbiRet::ok(0))
        );
        assert_eq!(h.timer, Some(12345));
        // u64::MAX (cancel) is passed through to the host.
        dispatch(&mut h, &call(EID_TIME, 0, [u64::MAX, 0, 0, 0, 0, 0]));
        assert_eq!(h.timer, Some(u64::MAX));
    }

    #[test]
    fn ipi_send() {
        let mut h = host();
        assert_eq!(
            dispatch(&mut h, &call(EID_IPI, 0, [0b110, 0, 0, 0, 0, 0])),
            SbiAction::Return(SbiRet::ok(0))
        );
        assert_eq!(h.ipis, vec![1, 2]);
        // all-harts shorthand
        h.ipis.clear();
        dispatch(&mut h, &call(EID_IPI, 0, [0, u64::MAX, 0, 0, 0, 0]));
        assert_eq!(h.ipis, vec![0, 1, 2, 3]);
        // invalid mask
        assert_eq!(
            dispatch(&mut h, &call(EID_IPI, 0, [0b1, 9, 0, 0, 0, 0])),
            SbiAction::Return(SbiRet::err(SBI_ERR_INVALID_PARAM))
        );
    }

    #[test]
    fn rfence_variants() {
        let mut h = host();
        for fid in 0..=2 {
            assert_eq!(
                dispatch(&mut h, &call(EID_RFENCE, fid, [0b11, 0, 0, 0x1000, 0, 0])),
                SbiAction::Return(SbiRet::ok(0))
            );
        }
        assert_eq!(h.fences, vec![vec![0, 1]; 3]);
        // hfence variants unsupported
        assert_eq!(
            dispatch(&mut h, &call(EID_RFENCE, 3, [0b1, 0, 0, 0, 0, 0])),
            SbiAction::Return(SbiRet::err(SBI_ERR_NOT_SUPPORTED))
        );
    }

    #[test]
    fn hsm_lifecycle() {
        let mut h = host();
        assert_eq!(
            dispatch(&mut h, &call(EID_HSM, 0, [2, 0x8020_0000, 0xfd7, 0, 0, 0])),
            SbiAction::Return(SbiRet::ok(0))
        );
        assert_eq!(h.started, vec![(2, 0x8020_0000, 0xfd7)]);
        // double-start reports ALREADY_AVAILABLE
        assert_eq!(
            dispatch(&mut h, &call(EID_HSM, 0, [2, 0x8020_0000, 0, 0, 0, 0])),
            SbiAction::Return(SbiRet::err(SBI_ERR_ALREADY_AVAILABLE))
        );
        // invalid hartid
        assert_eq!(
            dispatch(&mut h, &call(EID_HSM, 0, [4, 0, 0, 0, 0, 0])),
            SbiAction::Return(SbiRet::err(SBI_ERR_INVALID_PARAM))
        );
        // status
        assert_eq!(
            dispatch(&mut h, &call(EID_HSM, 2, [2, 0, 0, 0, 0, 0])),
            SbiAction::Return(SbiRet::ok(HSM_STATE_STARTED))
        );
        assert_eq!(
            dispatch(&mut h, &call(EID_HSM, 2, [1, 0, 0, 0, 0, 0])),
            SbiAction::Return(SbiRet::ok(HSM_STATE_STOPPED))
        );
        assert_eq!(
            dispatch(&mut h, &call(EID_HSM, 2, [4, 0, 0, 0, 0, 0])),
            SbiAction::Return(SbiRet::err(SBI_ERR_INVALID_PARAM))
        );
        // stop parks the calling hart
        assert_eq!(
            dispatch(&mut h, &call(EID_HSM, 1, [0; 6])),
            SbiAction::StopHart
        );
        // suspend unsupported
        assert_eq!(
            dispatch(&mut h, &call(EID_HSM, 3, [0; 6])),
            SbiAction::Return(SbiRet::err(SBI_ERR_NOT_SUPPORTED))
        );
    }

    #[test]
    fn srst_shutdown() {
        let mut h = host();
        for ty in 0..=2 {
            assert_eq!(
                dispatch(&mut h, &call(EID_SRST, 0, [ty, 0, 0, 0, 0, 0])),
                SbiAction::Shutdown
            );
        }
        assert_eq!(
            dispatch(&mut h, &call(EID_SRST, 0, [3, 0, 0, 0, 0, 0])),
            SbiAction::Return(SbiRet::err(SBI_ERR_NOT_SUPPORTED))
        );
    }

    #[test]
    fn dbcn_console() {
        let mut h = host();
        h.guest_mem = b"hello, sbi".to_vec();
        // write 5 bytes from gpa 0x1000
        assert_eq!(
            dispatch(&mut h, &call(EID_DBCN, 0, [5, 0x1000, 0, 0, 0, 0])),
            SbiAction::Return(SbiRet::ok(5))
        );
        assert_eq!(h.console, b"hello");
        // non-zero hi half is invalid on RV64
        assert_eq!(
            dispatch(&mut h, &call(EID_DBCN, 0, [1, 0x1000, 1, 0, 0, 0])),
            SbiAction::Return(SbiRet::err(SBI_ERR_INVALID_PARAM))
        );
        // out-of-bounds address
        assert_eq!(
            dispatch(&mut h, &call(EID_DBCN, 0, [4, 0x10_0000, 0, 0, 0, 0])),
            SbiAction::Return(SbiRet::err(SBI_ERR_INVALID_ADDRESS))
        );
        // write_byte
        h.console.clear();
        assert_eq!(
            dispatch(&mut h, &call(EID_DBCN, 2, [u64::from(b'X'), 0, 0, 0, 0, 0])),
            SbiAction::Return(SbiRet::ok(0))
        );
        assert_eq!(h.console, b"X");
        // console_read: zero bytes available
        assert_eq!(
            dispatch(&mut h, &call(EID_DBCN, 1, [4, 0x1000, 0, 0, 0, 0])),
            SbiAction::Return(SbiRet::ok(0))
        );
    }

    /// The v0.1 console is opt-in: a host that does not want it must neither
    /// serve it nor advertise it, or the guest will pick it up from the probe.
    #[test]
    fn legacy_console_is_opt_in() {
        let mut h = host();
        for eid in [EID_LEGACY_PUTCHAR, EID_LEGACY_GETCHAR] {
            assert_eq!(
                dispatch(&mut h, &call(EID_BASE, 3, [eid, 0, 0, 0, 0, 0])),
                SbiAction::Return(SbiRet::ok(0))
            );
            assert_eq!(
                dispatch(&mut h, &call(eid, 0, [u64::from(b'Z'), 0, 0, 0, 0, 0])),
                SbiAction::Return(SbiRet::err(SBI_ERR_NOT_SUPPORTED))
            );
        }
        assert!(h.console.is_empty());

        h.legacy = true;
        for eid in [EID_LEGACY_PUTCHAR, EID_LEGACY_GETCHAR] {
            assert_eq!(
                dispatch(&mut h, &call(EID_BASE, 3, [eid, 0, 0, 0, 0, 0])),
                SbiAction::Return(SbiRet::ok(1))
            );
        }
        assert_eq!(
            dispatch(&mut h, &call(EID_LEGACY_PUTCHAR, 0, [u64::from(b'Z'); 6])),
            SbiAction::Return(SbiRet::ok(0))
        );
        assert_eq!(h.console, b"Z");
        // getchar has no input source: -1 in a0 means "none".
        assert_eq!(
            dispatch(&mut h, &call(EID_LEGACY_GETCHAR, 0, [0; 6])),
            SbiAction::Return(SbiRet {
                error: -1,
                value: 0
            })
        );
    }
}
