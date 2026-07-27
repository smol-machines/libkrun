// Copyright 2026 The libkrun Authors. Licensed under Apache-2.0.
//
// Runs the official riscv-tests ISA binaries against the emulator via an
// ELF + HTIF (tohost) harness. Set RISCV_TESTS_DIR to the directory holding
// the rv64ui-p-* ELFs (riscv-tests/isa after `make isa`); the tests skip
// themselves when it is unset.

use std::path::{Path, PathBuf};
use std::{env, fs};

use krun_emu::{Clock, Cpu, GuestRam, VmExit, elf};

const RAM_BASE: u64 = 0x8000_0000;
const RAM_SIZE: usize = 256 << 20;
const MAX_INSNS: u64 = 200_000_000;
const BATCH: u64 = 4096;

struct TestMachine {
    cpu: Cpu,
    ram: GuestRam,
    tohost: u64,
    console: String,
}

enum Outcome {
    Pass,
    Fail(u64),
    Timeout,
    Stuck(String),
}

impl TestMachine {
    fn load(path: &Path, misa_mask: u64) -> Result<Self, String> {
        let image = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let ram = GuestRam::new_owned(RAM_BASE, RAM_SIZE);
        let entry = elf::load(&image, &ram).map_err(|e| format!("elf: {e:?}"))?;
        let tohost =
            elf::find_symbol(&image, "tohost").ok_or_else(|| "no tohost symbol".to_string())?;
        let mut cpu = Cpu::new(0, ram.clone(), Clock::Deterministic { shift: 3 });
        cpu.set_pc(entry);
        cpu.set_tohost(Some(tohost));
        cpu.mask_misa(misa_mask);
        Ok(TestMachine {
            cpu,
            ram,
            tohost,
            console: String::new(),
        })
    }

    fn run(&mut self) -> Outcome {
        loop {
            match self.cpu.run(BATCH) {
                VmExit::Shutdown { code } => match self.handle_tohost(code) {
                    Some(outcome) => return outcome,
                    None => continue,
                },
                VmExit::InstrLimit | VmExit::Wfi | VmExit::Interrupted => {
                    if self.cpu.instret() > MAX_INSNS {
                        return Outcome::Timeout;
                    }
                }
                // riscv-tests live entirely in RAM; tolerate stray MMIO by
                // reading zeros and dropping writes.
                VmExit::MmioRead { .. } => self.cpu.complete_mmio_read(0),
                VmExit::MmioWrite { .. } => {}
                VmExit::Ecall { from } => {
                    return Outcome::Stuck(format!("unexpected SBI ecall from {from:?}"));
                }
            }
        }
    }

    /// HTIF: value 1 = pass, other odd = fail (test number in bits 63:1),
    /// device 1 cmd 1 = console putchar.
    fn handle_tohost(&mut self, code: u64) -> Option<Outcome> {
        if code == 1 {
            return Some(Outcome::Pass);
        }
        if code & 1 == 1 && code >> 48 == 0 {
            return Some(Outcome::Fail(code >> 1));
        }
        if code >> 48 == 0x0101 {
            self.console.push((code & 0xff) as u8 as char);
            self.ram.store(self.tohost, 8, 0).unwrap();
            return None;
        }
        Some(Outcome::Stuck(format!("unhandled tohost value {code:#x}")))
    }
}

fn console_tail(m: &TestMachine) -> String {
    if m.console.is_empty() {
        String::new()
    } else {
        format!(" console: {:?}", m.console)
    }
}

fn run_suite(prefix: &str) -> Result<(), String> {
    let Some(dir) = env::var_os("RISCV_TESTS_DIR") else {
        eprintln!("RISCV_TESTS_DIR not set; skipping {prefix}* suite");
        return Ok(());
    };
    // misa advertises F/D: rv64mi-p-csr then executes FP instructions and the
    // -v environment saves FP trap state, so leaving the bits set is the
    // stronger test. (They were masked while the FPU was still a stub.)
    let misa_mask = 0;
    let mut elfs: Vec<PathBuf> = fs::read_dir(&dir)
        .map_err(|e| format!("read_dir {dir:?}: {e}"))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_none()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(prefix))
        })
        .collect();
    elfs.sort();
    if elfs.is_empty() {
        return Err(format!("no {prefix}* binaries in {dir:?}"));
    }

    let mut failures = Vec::new();
    for path in &elfs {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let result = TestMachine::load(path, misa_mask).map(|mut m| (m.run(), m));
        match result {
            Ok((Outcome::Pass, m)) => {
                println!("{name:<24} ok ({} instrs)", m.cpu.instret());
            }
            Ok((Outcome::Fail(n), m)) => {
                println!("{name:<24} FAIL (test {n}){}", console_tail(&m));
                failures.push(format!("{name}: test case {n}"));
            }
            Ok((Outcome::Timeout, m)) => {
                println!(
                    "{name:<24} TIMEOUT (pc={:#x}){}",
                    m.cpu.pc,
                    console_tail(&m)
                );
                failures.push(format!("{name}: timeout after {MAX_INSNS} instrs"));
            }
            Ok((Outcome::Stuck(why), m)) => {
                println!(
                    "{name:<24} STUCK: {why} (pc={:#x}){}",
                    m.cpu.pc,
                    console_tail(&m)
                );
                failures.push(format!("{name}: {why}"));
            }
            Err(e) => {
                println!("{name:<24} ERROR: {e}");
                failures.push(format!("{name}: {e}"));
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{}/{} tests failed:\n  {}",
            failures.len(),
            elfs.len(),
            failures.join("\n  ")
        ))
    }
}

#[test]
fn rv64ui_p() {
    if let Err(e) = run_suite("rv64ui-p-") {
        panic!("{e}");
    }
}

#[test]
fn rv64um_p() {
    if let Err(e) = run_suite("rv64um-p-") {
        panic!("{e}");
    }
}

#[test]
fn rv64ua_p() {
    if let Err(e) = run_suite("rv64ua-p-") {
        panic!("{e}");
    }
}

#[test]
fn rv64uc_p() {
    if let Err(e) = run_suite("rv64uc-p-") {
        panic!("{e}");
    }
}

#[test]
fn rv64si_p() {
    if let Err(e) = run_suite("rv64si-p-") {
        panic!("{e}");
    }
}

#[test]
fn rv64mi_p() {
    if let Err(e) = run_suite("rv64mi-p-") {
        panic!("{e}");
    }
}

// The -v variants run the tests in U-mode under Sv39 with a demand-paging
// supervisor; they are the MMU acceptance suite.

#[test]
fn rv64ui_v() {
    if let Err(e) = run_suite("rv64ui-v-") {
        panic!("{e}");
    }
}

#[test]
fn rv64um_v() {
    if let Err(e) = run_suite("rv64um-v-") {
        panic!("{e}");
    }
}

#[test]
fn rv64ua_v() {
    if let Err(e) = run_suite("rv64ua-v-") {
        panic!("{e}");
    }
}

#[test]
fn rv64uc_v() {
    if let Err(e) = run_suite("rv64uc-v-") {
        panic!("{e}");
    }
}

#[test]
fn rv64uf_p() {
    if let Err(e) = run_suite("rv64uf-p-") {
        panic!("{e}");
    }
}

#[test]
fn rv64ud_p() {
    if let Err(e) = run_suite("rv64ud-p-") {
        panic!("{e}");
    }
}

#[test]
fn rv64uf_v() {
    if let Err(e) = run_suite("rv64uf-v-") {
        panic!("{e}");
    }
}

#[test]
fn rv64ud_v() {
    if let Err(e) = run_suite("rv64ud-v-") {
        panic!("{e}");
    }
}
