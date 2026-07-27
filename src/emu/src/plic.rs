// Copyright 2026 The libkrun Authors. Licensed under Apache-2.0.

//! SiFive PLIC state machine, shared by every embedder of the emulated CPU
//! (the native `emu` backend wraps it in `devices::legacy::PlicDevice`, the
//! wasm machine drives it directly).
//!
//! Models the interrupt controller Linux's `irq-sifive-plic` driver expects
//! (compatible `"sifive,plic-1.0.0"` / `"riscv,plic0"`), with one context per
//! hart wired to that hart's S-mode external interrupt (SEIP). Register map,
//! offsets from the PLIC base (32-bit accesses only):
//!
//! | Offset                    | Register                                   |
//! |---------------------------|--------------------------------------------|
//! | `0x00_0000 + 4*src`       | priority of source `src` (1..=n, 0..=7)    |
//! | `0x00_1000 + 4*word`      | pending bits (bit index == source ID, RO)  |
//! | `0x00_2000 + 0x80*ctx+4*w`| per-context enable bits                    |
//! | `0x20_0000 + 0x1000*ctx`  | per-context priority threshold             |
//! | `0x20_0000 + 0x1000*ctx+4`| claim (read) / complete (write)            |
//!
//! Sources are level-triggered. Following the RISC-V PLIC spec, a pending bit
//! is only ever cleared by a claim, the gateway forwards no new request for a
//! source while one is in flight (claimed but not completed), and a completion
//! re-pends the source if its level is still asserted. A claim is not gated by
//! the threshold (the threshold only masks the EIP notification), and a
//! completion whose ID is not enabled for the completing context is ignored
//! (Linux relies on this: `plic_irq_eoi` briefly re-enables a disabled source
//! to complete it).
//!
//! Two ways to assert a source exist:
//!
//! - [`Plic::set_source`]`(src, level)`: a true level line. The level stays
//!   asserted until the caller drops it; completion re-pends while it is high
//!   (16550-style devices whose IIR state is the level).
//! - [`Plic::signal_source`]`(src)`: assert, then auto-deassert on claim.
//!   Virtio-mmio interrupts are level interrupts driven by the ISR status
//!   register, but the transport only signals on 0->1 status transitions and
//!   cannot tell us when the guest's ISR read drops the level, so — like the
//!   HVF userspace GICv3, which pends the intid once and dedupes until
//!   acknowledged — the claim is treated as the acknowledgement. A re-signal
//!   between claim and completion re-asserts the level, so the completion
//!   re-pends it and no interrupt is lost.
//!
//! Interrupt delivery is the embedder's job: after any operation that can
//! change a context's output, read [`Plic::hart_ext_pending`] and drive that
//! hart's SEIP line (`Cpu::set_irq(IrqLine::SExt, ..)`).
//!
//! This is a pure state machine: no threads, no locks, no callbacks. Accesses
//! are assumed to be naturally-aligned 32-bit ones; policing the access size
//! belongs to the embedder's MMIO layer.

/// Priorities span 0..=7 (0 = "never interrupt"), like QEMU's virt machine.
const MAX_PRIORITY: u32 = 7;

/// Contexts the 4 MiB aperture has room for: (0x40_0000 - 0x20_0000) / 0x1000.
const MAX_CONTEXTS: usize = 512;

/// Sources the register map has room for: the priority array ends at
/// [`PENDING_BASE`] and one context's enable block spans [`ENABLE_STRIDE`].
const MAX_SOURCES: u32 = 1023;

// Register map (offsets from the PLIC base).
const PRIORITY_BASE: u64 = 0x0;
const PENDING_BASE: u64 = 0x1000;
const ENABLE_BASE: u64 = 0x2000;
const ENABLE_STRIDE: u64 = 0x80;
const CONTEXT_BASE: u64 = 0x0020_0000;
const CONTEXT_STRIDE: u64 = 0x1000;
const CONTEXT_END: u64 = 0x0040_0000;
const CONTEXT_THRESHOLD: u64 = 0x0;
const CONTEXT_CLAIM: u64 = 0x4;

/// One context per hart: context i = hart i, S-mode external interrupt.
struct Context {
    enable: Vec<u32>,
    threshold: u32,
}

pub struct Plic {
    /// Valid source IDs are `1..=num_sources`; source 0 is reserved.
    num_sources: u32,
    /// Per-source priority (index 0 unused).
    priority: Vec<u32>,
    /// Externally driven level of each source line.
    level: Vec<bool>,
    /// Source was last asserted through [`Plic::signal_source`]: its level is
    /// dropped when the source is claimed (see module doc).
    auto_deassert: Vec<bool>,
    /// In-flight sources (claimed, not yet completed): the gateway forwards
    /// no new request for these until completion.
    claimed: Vec<bool>,
    /// Pending bits, bit index == source ID.
    pending: Vec<u32>,
    contexts: Vec<Context>,
}

impl Plic {
    /// Size of the MMIO aperture (matches the `reg` property of the FDT node).
    pub const MMIO_SIZE: u64 = CONTEXT_END;

    /// Source count the libkrun riscv64 FDT advertises as `riscv,ndev`.
    pub const DEFAULT_NUM_SOURCES: u32 = 96;

    pub fn new(vcpu_count: usize, num_sources: u32) -> Self {
        assert!(
            vcpu_count <= MAX_CONTEXTS,
            "PLIC supports at most {MAX_CONTEXTS} harts"
        );
        assert!(
            num_sources <= MAX_SOURCES,
            "PLIC supports at most {MAX_SOURCES} sources"
        );
        let num_words = (num_sources as usize + 1).div_ceil(32);
        let contexts = (0..vcpu_count)
            .map(|_| Context {
                enable: vec![0; num_words],
                threshold: 0,
            })
            .collect();
        Plic {
            num_sources,
            priority: vec![0; num_sources as usize + 1],
            level: vec![false; num_sources as usize + 1],
            auto_deassert: vec![false; num_sources as usize + 1],
            claimed: vec![false; num_sources as usize + 1],
            pending: vec![0; num_words],
            contexts,
        }
    }

    /// Read a 32-bit register. A claim read has side effects.
    pub fn read(&mut self, offset: u64) -> u32 {
        match offset {
            o if (PRIORITY_BASE..PENDING_BASE).contains(&o) => {
                let src = (o / 4) as usize;
                if self.valid_source(src) {
                    self.priority[src]
                } else {
                    0
                }
            }
            o if (PENDING_BASE..ENABLE_BASE).contains(&o) => {
                let word = ((o - PENDING_BASE) / 4) as usize;
                self.pending.get(word).copied().unwrap_or(0)
            }
            o if (ENABLE_BASE..CONTEXT_BASE).contains(&o) => {
                let ctx = ((o - ENABLE_BASE) / ENABLE_STRIDE) as usize;
                let word = (((o - ENABLE_BASE) % ENABLE_STRIDE) / 4) as usize;
                match self.contexts.get(ctx) {
                    Some(c) => c.enable.get(word).copied().unwrap_or(0),
                    None => 0,
                }
            }
            o if (CONTEXT_BASE..CONTEXT_END).contains(&o) => {
                let ctx = ((o - CONTEXT_BASE) / CONTEXT_STRIDE) as usize;
                if ctx >= self.contexts.len() {
                    return 0;
                }
                match (o - CONTEXT_BASE) % CONTEXT_STRIDE {
                    CONTEXT_THRESHOLD => self.contexts[ctx].threshold,
                    CONTEXT_CLAIM => self.claim(ctx),
                    _ => 0,
                }
            }
            _ => 0,
        }
    }

    /// Write a 32-bit register.
    pub fn write(&mut self, offset: u64, value: u32) {
        match offset {
            o if (PRIORITY_BASE..PENDING_BASE).contains(&o) => {
                let src = (o / 4) as usize;
                if self.valid_source(src) {
                    self.priority[src] = value & MAX_PRIORITY;
                }
            }
            o if (PENDING_BASE..ENABLE_BASE).contains(&o) => {
                // Pending bits are read-only; set by the gateway, cleared by claim.
            }
            o if (ENABLE_BASE..CONTEXT_BASE).contains(&o) => {
                let ctx = ((o - ENABLE_BASE) / ENABLE_STRIDE) as usize;
                let word = (((o - ENABLE_BASE) % ENABLE_STRIDE) / 4) as usize;
                let mask = self.valid_sources_mask(word);
                if let Some(c) = self.contexts.get_mut(ctx)
                    && let Some(w) = c.enable.get_mut(word)
                {
                    *w = value & mask;
                }
            }
            o if (CONTEXT_BASE..CONTEXT_END).contains(&o) => {
                let ctx = ((o - CONTEXT_BASE) / CONTEXT_STRIDE) as usize;
                if ctx >= self.contexts.len() {
                    return;
                }
                match (o - CONTEXT_BASE) % CONTEXT_STRIDE {
                    // Linux writes 0x7fffffff to park a context; masking to the
                    // priority range keeps that meaning (7 masks everything).
                    CONTEXT_THRESHOLD => self.contexts[ctx].threshold = value & MAX_PRIORITY,
                    CONTEXT_CLAIM => self.complete(ctx, value),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// Drive `source`'s line as a true level. Out-of-range sources are
    /// ignored.
    pub fn set_source(&mut self, source: u32, level: bool) {
        self.set_level(source, level, false);
    }

    /// Assert `source` with auto-deassert-on-claim semantics (see module doc).
    pub fn signal_source(&mut self, source: u32) {
        self.set_level(source, true, true);
    }

    /// `hart`'s S-mode external interrupt output: any pending+enabled source
    /// above its context threshold.
    pub fn hart_ext_pending(&self, hart: usize) -> bool {
        let Some(ctx) = self.contexts.get(hart) else {
            return false;
        };
        (1..=self.num_sources as usize).any(|src| {
            self.pending_bit(src)
                && ctx.enable[src / 32] & (1 << (src % 32)) != 0
                && self.priority[src] > ctx.threshold
        })
    }

    fn valid_source(&self, src: usize) -> bool {
        (1..=self.num_sources as usize).contains(&src)
    }

    /// Mask of bits in enable/pending `word` that correspond to valid source
    /// IDs.
    fn valid_sources_mask(&self, word: usize) -> u32 {
        let mut mask = 0u32;
        for bit in 0..32usize {
            let src = word * 32 + bit;
            if self.valid_source(src) {
                mask |= 1 << bit;
            }
        }
        mask
    }

    fn pending_bit(&self, src: usize) -> bool {
        self.pending[src / 32] & (1 << (src % 32)) != 0
    }

    fn set_pending_bit(&mut self, src: usize, val: bool) {
        if val {
            self.pending[src / 32] |= 1 << (src % 32);
        } else {
            self.pending[src / 32] &= !(1 << (src % 32));
        }
    }

    fn enabled(&self, ctx: usize, src: usize) -> bool {
        self.contexts[ctx].enable[src / 32] & (1 << (src % 32)) != 0
    }

    /// Level-triggered gateway: a high level forwards a request unless one is
    /// already in flight; pending is only ever cleared by a claim (a dropped
    /// level leaves it set).
    fn set_level(&mut self, source: u32, level: bool, auto: bool) {
        let src = source as usize;
        if !self.valid_source(src) {
            return;
        }
        self.level[src] = level;
        self.auto_deassert[src] = auto;
        if level && !self.claimed[src] {
            self.set_pending_bit(src, true);
        }
    }

    /// Claim for `ctx`: the pending+enabled source with the highest nonzero
    /// priority (lowest ID on ties), or 0. Not gated by the threshold.
    fn claim(&mut self, ctx: usize) -> u32 {
        let mut best = 0usize;
        let mut best_prio = 0u32;
        for src in 1..=self.num_sources as usize {
            if self.pending_bit(src) && self.enabled(ctx, src) && self.priority[src] > best_prio {
                best = src;
                best_prio = self.priority[src];
            }
        }
        if best != 0 {
            self.set_pending_bit(best, false);
            self.claimed[best] = true;
            if self.auto_deassert[best] {
                self.level[best] = false;
            }
        }
        best as u32
    }

    /// Complete `id` for `ctx`. Ignored unless `id` is a claimed source that
    /// is enabled for this context (see module doc). Re-pends the source if
    /// its level is still asserted.
    fn complete(&mut self, ctx: usize, id: u32) {
        let src = id as usize;
        if !self.valid_source(src) || !self.enabled(ctx, src) || !self.claimed[src] {
            return;
        }
        self.claimed[src] = false;
        if self.level[src] {
            self.set_pending_bit(src, true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plic(vcpu_count: usize) -> Plic {
        Plic::new(vcpu_count, Plic::DEFAULT_NUM_SOURCES)
    }

    fn claim_offset(ctx: u64) -> u64 {
        CONTEXT_BASE + ctx * CONTEXT_STRIDE + CONTEXT_CLAIM
    }

    fn threshold_offset(ctx: u64) -> u64 {
        CONTEXT_BASE + ctx * CONTEXT_STRIDE + CONTEXT_THRESHOLD
    }

    fn enable_offset(ctx: u64, word: u64) -> u64 {
        ENABLE_BASE + ctx * ENABLE_STRIDE + 4 * word
    }

    fn claim(plic: &mut Plic, ctx: u64) -> u32 {
        plic.read(claim_offset(ctx))
    }

    fn complete(plic: &mut Plic, ctx: u64, id: u32) {
        plic.write(claim_offset(ctx), id);
    }

    /// Enable `src` for `ctx` (read-modify-write of the enable word).
    fn enable_source(plic: &mut Plic, ctx: u64, src: u32) {
        let offset = enable_offset(ctx, u64::from(src) / 32);
        let word = plic.read(offset);
        plic.write(offset, word | 1 << (src % 32));
    }

    fn set_priority(plic: &mut Plic, src: u32, prio: u32) {
        plic.write(PRIORITY_BASE + 4 * u64::from(src), prio);
    }

    fn pending_bit(plic: &mut Plic, src: u32) -> bool {
        plic.read(PENDING_BASE + 4 * (u64::from(src) / 32)) & (1 << (src % 32)) != 0
    }

    #[test]
    fn priority_ordering_and_threshold() {
        let mut plic = plic(1);
        for src in [10u32, 11, 12] {
            enable_source(&mut plic, 0, src);
        }
        set_priority(&mut plic, 10, 3);
        set_priority(&mut plic, 11, 5);
        set_priority(&mut plic, 12, 5);
        assert!(!plic.hart_ext_pending(0));

        plic.set_source(10, true);
        plic.set_source(11, true);
        plic.set_source(12, true);
        assert!(plic.hart_ext_pending(0));

        // Highest priority first; lowest ID wins the 11-vs-12 tie.
        assert_eq!(claim(&mut plic, 0), 11);
        assert_eq!(claim(&mut plic, 0), 12);
        assert!(plic.hart_ext_pending(0));
        assert_eq!(claim(&mut plic, 0), 10);
        // All pending consumed: EIP dropped with the last claim.
        assert!(!plic.hart_ext_pending(0));
        assert_eq!(claim(&mut plic, 0), 0);

        // Threshold masks EIP but does not gate the claim itself.
        plic.set_source(10, false);
        plic.set_source(11, false);
        plic.set_source(12, false);
        for src in [10u32, 11, 12] {
            complete(&mut plic, 0, src);
        }
        plic.write(threshold_offset(0), 5);
        plic.set_source(11, true); // priority 5 == threshold: masked
        assert!(!plic.hart_ext_pending(0));
        assert!(pending_bit(&mut plic, 11));
        assert_eq!(claim(&mut plic, 0), 11);

        // Lowering the threshold with a claimable source pending raises EIP.
        plic.set_source(10, true);
        assert!(!plic.hart_ext_pending(0));
        plic.write(threshold_offset(0), 2);
        assert!(plic.hart_ext_pending(0));
    }

    #[test]
    fn enable_masking() {
        let mut plic = plic(1);
        set_priority(&mut plic, 5, 1);
        plic.set_source(5, true);

        // Pending but not enabled: visible in the pending register, no EIP,
        // not claimable.
        assert!(pending_bit(&mut plic, 5));
        assert!(!plic.hart_ext_pending(0));
        assert_eq!(claim(&mut plic, 0), 0);

        // Enabling an already-pending source raises EIP and makes it claimable.
        enable_source(&mut plic, 0, 5);
        assert!(plic.hart_ext_pending(0));
        assert_eq!(claim(&mut plic, 0), 5);
        assert!(!plic.hart_ext_pending(0));
    }

    #[test]
    fn two_harts() {
        let mut plic = plic(2);
        set_priority(&mut plic, 7, 4);
        set_priority(&mut plic, 8, 4);
        enable_source(&mut plic, 0, 7);
        enable_source(&mut plic, 1, 7);
        enable_source(&mut plic, 1, 8);

        // Source 8 is enabled only for hart 1.
        plic.set_source(8, true);
        assert!(!plic.hart_ext_pending(0));
        assert!(plic.hart_ext_pending(1));
        assert_eq!(claim(&mut plic, 0), 0);

        // Source 7 targets both harts.
        plic.set_source(7, true);
        assert!(plic.hart_ext_pending(0));

        // Hart 0 claims 7 (its only candidate); hart 1 still sees 8.
        assert_eq!(claim(&mut plic, 0), 7);
        assert!(!plic.hart_ext_pending(0));
        assert!(plic.hart_ext_pending(1));
        assert_eq!(claim(&mut plic, 1), 8);
        assert!(!plic.hart_ext_pending(1));

        // Per-context thresholds are independent.
        plic.set_source(7, false);
        complete(&mut plic, 0, 7);
        plic.set_source(8, false);
        complete(&mut plic, 1, 8);
        plic.write(threshold_offset(1), 7);
        plic.set_source(7, true);
        // Hart 0 (threshold 0) is interrupted, hart 1 (threshold 7) is not.
        assert!(plic.hart_ext_pending(0));
        assert!(!plic.hart_ext_pending(1));
        // A hart with no context never has an output.
        assert!(!plic.hart_ext_pending(2));
    }

    #[test]
    fn level_reassert_on_complete() {
        let mut plic = plic(1);
        set_priority(&mut plic, 3, 1);
        enable_source(&mut plic, 0, 3);

        // True level source held high across claim+complete: re-pends.
        plic.set_source(3, true);
        assert!(plic.hart_ext_pending(0));
        assert_eq!(claim(&mut plic, 0), 3);
        assert!(!pending_bit(&mut plic, 3));
        assert!(!plic.hart_ext_pending(0));

        // While in flight, the gateway holds new requests: the line staying
        // (or going back) high must not re-pend before completion.
        plic.set_source(3, true);
        assert!(!pending_bit(&mut plic, 3));
        assert!(!plic.hart_ext_pending(0));

        complete(&mut plic, 0, 3);
        assert!(pending_bit(&mut plic, 3));
        assert!(plic.hart_ext_pending(0));

        // Line dropped before completion: no re-pend.
        assert_eq!(claim(&mut plic, 0), 3);
        plic.set_source(3, false);
        complete(&mut plic, 0, 3);
        assert!(!pending_bit(&mut plic, 3));
        assert!(!plic.hart_ext_pending(0));
        assert_eq!(claim(&mut plic, 0), 0);
    }

    #[test]
    fn signal_source_auto_deasserts_on_claim() {
        let mut plic = plic(1);
        set_priority(&mut plic, 2, 1);
        enable_source(&mut plic, 0, 2);

        // Virtio-style assertion: claim acknowledges it, completion with no
        // re-signal in between must not re-pend.
        plic.signal_source(2);
        assert!(plic.hart_ext_pending(0));
        assert_eq!(claim(&mut plic, 0), 2);
        assert!(!plic.hart_ext_pending(0));
        complete(&mut plic, 0, 2);
        assert!(!pending_bit(&mut plic, 2));

        // A re-signal between claim and completion re-pends at completion.
        plic.signal_source(2);
        assert_eq!(claim(&mut plic, 0), 2);
        plic.signal_source(2);
        assert!(!pending_bit(&mut plic, 2)); // held until completion
        complete(&mut plic, 0, 2);
        assert!(pending_bit(&mut plic, 2));
        assert_eq!(claim(&mut plic, 0), 2);
        complete(&mut plic, 0, 2);
        assert_eq!(claim(&mut plic, 0), 0);
    }

    #[test]
    fn completion_of_disabled_source_is_ignored() {
        let mut plic = plic(1);
        set_priority(&mut plic, 4, 1);
        enable_source(&mut plic, 0, 4);
        plic.set_source(4, true);
        assert_eq!(claim(&mut plic, 0), 4);

        // Disable the source, then complete: ignored, still in flight.
        plic.write(enable_offset(0, 0), 0);
        complete(&mut plic, 0, 4);
        assert!(!pending_bit(&mut plic, 4));

        // Re-enable and complete: the still-high level re-pends.
        enable_source(&mut plic, 0, 4);
        complete(&mut plic, 0, 4);
        assert!(pending_bit(&mut plic, 4));

        // Completing a source that was never claimed is a no-op.
        complete(&mut plic, 0, 9);
        // Bogus IDs are ignored.
        complete(&mut plic, 0, 0);
        complete(&mut plic, 0, Plic::DEFAULT_NUM_SOURCES + 1);
        assert_eq!(claim(&mut plic, 0), 4);
    }

    #[test]
    fn register_access_edge_cases() {
        let mut plic = plic(1);

        // Priority writes are masked to 0..=7; source 0's slot is reserved.
        set_priority(&mut plic, 1, 0xffff_ffff);
        assert_eq!(plic.read(PRIORITY_BASE + 4), 7);
        plic.write(PRIORITY_BASE, 5);
        assert_eq!(plic.read(PRIORITY_BASE), 0);

        // Threshold writes are masked; Linux's 0x7fffffff "park" still masks
        // every priority.
        plic.write(threshold_offset(0), 0x7fff_ffff);
        assert_eq!(plic.read(threshold_offset(0)), 7);
        plic.write(threshold_offset(0), 0);

        // Enable writes: bit 0 of word 0 (source 0) and bits beyond
        // num_sources in the last word cannot be set.
        plic.write(enable_offset(0, 0), 0xffff_ffff);
        assert_eq!(plic.read(enable_offset(0, 0)), 0xffff_fffe);
        let last_word = plic.pending.len() - 1;
        plic.write(enable_offset(0, last_word as u64), 0xffff_ffff);
        assert_eq!(
            plic.read(enable_offset(0, last_word as u64)),
            plic.valid_sources_mask(last_word)
        );

        // Pending is read-only.
        set_priority(&mut plic, 1, 1);
        plic.write(PENDING_BASE, 0xffff_ffff);
        assert_eq!(plic.read(PENDING_BASE), 0);

        // Reserved/out-of-range offsets: reads return 0, writes are ignored.
        assert_eq!(plic.read(PENDING_BASE + 4 * plic.pending.len() as u64), 0);
        assert_eq!(plic.read(CONTEXT_BASE + 8), 0);
        plic.write(CONTEXT_BASE + 8, 0xdead);
        assert_eq!(plic.read(CONTEXT_END), 0);
        plic.write(CONTEXT_END, 0xdead);
        // Accesses to contexts beyond vcpu_count are safely ignored.
        assert_eq!(plic.read(threshold_offset(1)), 0);
        assert_eq!(claim(&mut plic, 1), 0);
        plic.write(threshold_offset(1), 3);
        plic.write(enable_offset(1, 0), 0xffff_ffff);

        assert!(!plic.hart_ext_pending(0));
    }

    #[test]
    fn source_count_is_configurable() {
        let mut plic = Plic::new(1, 8);
        // Only 1..=8 exist: the enable mask, the priority array and the
        // gateway all stop there.
        plic.write(enable_offset(0, 0), 0xffff_ffff);
        assert_eq!(plic.read(enable_offset(0, 0)), 0x1fe);
        set_priority(&mut plic, 9, 7);
        assert_eq!(plic.read(PRIORITY_BASE + 4 * 9), 0);
        plic.set_source(9, true);
        assert!(!plic.hart_ext_pending(0));

        set_priority(&mut plic, 8, 1);
        plic.set_source(8, true);
        assert!(plic.hart_ext_pending(0));
        assert_eq!(claim(&mut plic, 0), 8);
    }
}
