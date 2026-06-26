// Copyright 2026 The libkrun Authors. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Minimal i8254 PIT (channel 0 only) for the WHP backend.
//!
//! WHP has no PIT, and its emulated LAPIC timer does not deliver interrupts on
//! the WHP builds we target, so a guest is left with no working clockevent —
//! `nanosleep`, scheduling and timeouts hang. This device emulates the i8254
//! channel-0 counter wired to IRQ 0: it tracks the reload value and mode the
//! guest programs through ports 0x40/0x43, and a background thread asserts IRQ 0
//! through the IOAPIC at the programmed cadence. Combined with `nolapic_timer`
//! on the kernel command line (so the guest selects its i8253 clockevent instead
//! of the dead LAPIC timer), this gives the guest a working tick.
//!
//! Only channel 0 is modeled; channels 1 (legacy DRAM refresh) and 2 (PC
//! speaker) are not used by the Linux clockevent. Read-back of the live count is
//! not implemented (the clockevent reprograms the counter each tick in one-shot
//! mode and never depends on a precise read), so reads return 0.

use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use crate::bus::BusDevice;
use crate::legacy::IrqChip;

/// i8254 input clock: 1.193182 MHz.
const PIT_FREQ_HZ: u64 = 1_193_182;
/// Channel 0 is wired to IRQ 0.
const PIT_IRQ: u32 = 0;

/// Arm request sent to the timer thread when the guest finishes programming
/// channel 0.
struct Arm {
    period: Duration,
    periodic: bool,
}

/// i8254 channel-0 timer. Registered on the PIO bus at ports 0x40–0x43.
pub struct Pit {
    tx: Sender<Arm>,
    /// `true` when the access mode is lobyte/hibyte (two writes per reload).
    access_lohi: bool,
    /// In lobyte/hibyte mode, `true` once the low byte has been written and the
    /// high byte is expected next.
    expect_hi: bool,
    /// Latched low byte for lobyte/hibyte programming.
    reload_lo: u8,
    /// Channel-0 operating mode (0–5); modes 2 and 3 are periodic.
    mode: u8,
}

impl Pit {
    /// Creates the PIT and spawns its background interrupt thread, which asserts
    /// IRQ 0 through `intc` at the programmed cadence.
    pub fn new(intc: IrqChip) -> Self {
        let (tx, rx) = channel::<Arm>();
        thread::Builder::new()
            .name("pit-timer".into())
            .spawn(move || timer_loop(rx, intc))
            .expect("failed to spawn PIT timer thread");
        Pit {
            tx,
            access_lohi: true,
            expect_hi: false,
            reload_lo: 0,
            mode: 2,
        }
    }

    fn arm(&self, reload: u16) {
        let ticks = if reload == 0 { 65_536 } else { u64::from(reload) };
        let period_ns = (ticks * 1_000_000_000 / PIT_FREQ_HZ).max(1);
        let periodic = matches!(self.mode, 2 | 3);
        let _ = self.tx.send(Arm {
            period: Duration::from_nanos(period_ns),
            periodic,
        });
    }
}

impl BusDevice for Pit {
    fn write(&mut self, _vcpuid: u64, offset: u64, data: &[u8]) {
        let Some(&val) = data.first() else {
            return;
        };
        match offset {
            // Port 0x43: mode/command register.
            3 => {
                // Only channel 0 (bits 6-7 == 0) drives IRQ 0; ignore the rest.
                if val >> 6 != 0 {
                    return;
                }
                let access = (val >> 4) & 0b11;
                self.mode = (val >> 1) & 0b111;
                self.access_lohi = access == 0b11;
                self.expect_hi = false;
            }
            // Port 0x40: channel 0 counter (reload value).
            0 => {
                if self.access_lohi {
                    if self.expect_hi {
                        let reload = (u16::from(val) << 8) | u16::from(self.reload_lo);
                        self.expect_hi = false;
                        self.arm(reload);
                    } else {
                        self.reload_lo = val;
                        self.expect_hi = true;
                    }
                } else {
                    // Lobyte-only or hibyte-only access: a single write completes it.
                    self.arm(u16::from(val));
                }
            }
            _ => {}
        }
    }

    fn read(&mut self, _vcpuid: u64, _offset: u64, data: &mut [u8]) {
        data.fill(0);
    }
}

/// Background thread: assert IRQ 0 at the cadence the guest programs. A re-arm
/// (new `Arm` on the channel) preempts the current wait so the next deadline
/// takes effect immediately.
fn timer_loop(rx: std::sync::mpsc::Receiver<Arm>, intc: IrqChip) {
    let mut deadline: Option<Instant> = None;
    let mut period = Duration::from_millis(10);
    let mut periodic = false;

    loop {
        let wait = match deadline {
            Some(d) => d.saturating_duration_since(Instant::now()),
            None => Duration::from_secs(3600),
        };
        match rx.recv_timeout(wait) {
            Ok(arm) => {
                period = arm.period;
                periodic = arm.periodic;
                deadline = Some(Instant::now() + period);
            }
            Err(RecvTimeoutError::Timeout) => {
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    let _ = intc.lock().unwrap().set_irq(Some(PIT_IRQ), None);
                    // Re-arm from "now" rather than the old deadline so a slow
                    // host can't accumulate a backlog of catch-up ticks.
                    deadline = periodic.then(|| Instant::now() + period);
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}
