// Copyright 2026 The libkrun Authors. Licensed under Apache-2.0.
//
// Interpreter throughput benchmark. Runs a synthetic RV64GC guest kernel for
// a fixed instruction count and reports MIPS, with and without Sv39
// translation, so interpreter changes can be compared repeatably:
//
//     cargo test --release -p krun-emu --test bench -- --ignored --nocapture
//
// The guest loop mixes the instruction classes a kernel executes in bulk
// (loads, stores, ALU, multiply, a data-dependent branch, RVC forms) over a
// 1 MiB working set, so it walks enough pages to keep the TLB honest.

use std::time::Instant;

use krun_emu::{Clock, Cpu, GuestRam, PrivMode, VmExit};

const RAM_BASE: u64 = 0x8000_0000;
const RAM_SIZE: usize = 64 << 20;
/// Sv39 root page table (identity gigapages).
const PT_ROOT: u64 = RAM_BASE + (2 << 20);
/// Working set the guest loop reads and writes.
const BUF: u64 = RAM_BASE + (4 << 20);
const BUF_LEN: u64 = 1 << 20;

const INSNS: u64 = 60_000_000;

// ---- instruction encoders ----

fn r_type(f7: u32, rs2: u32, rs1: u32, f3: u32, rd: u32, op: u32) -> u32 {
    (f7 << 25) | (rs2 << 20) | (rs1 << 15) | (f3 << 12) | (rd << 7) | op
}

fn i_type(imm: i32, rs1: u32, f3: u32, rd: u32, op: u32) -> u32 {
    ((imm as u32 & 0xfff) << 20) | (rs1 << 15) | (f3 << 12) | (rd << 7) | op
}

fn s_type(imm: i32, rs2: u32, rs1: u32, f3: u32, op: u32) -> u32 {
    let u = imm as u32;
    (((u >> 5) & 0x7f) << 25) | (rs2 << 20) | (rs1 << 15) | (f3 << 12) | ((u & 0x1f) << 7) | op
}

fn b_type(imm: i32, rs2: u32, rs1: u32, f3: u32) -> u32 {
    let u = imm as u32;
    (((u >> 12) & 1) << 31)
        | (((u >> 5) & 0x3f) << 25)
        | (rs2 << 20)
        | (rs1 << 15)
        | (f3 << 12)
        | (((u >> 1) & 0xf) << 8)
        | (((u >> 11) & 1) << 7)
        | 0x63
}

fn j_type(imm: i32, rd: u32) -> u32 {
    let u = imm as u32;
    (((u >> 20) & 1) << 31)
        | (((u >> 1) & 0x3ff) << 21)
        | (((u >> 11) & 1) << 20)
        | (((u >> 12) & 0xff) << 12)
        | (rd << 7)
        | 0x6f
}

fn addi(rd: u32, rs1: u32, imm: i32) -> u32 {
    i_type(imm, rs1, 0, rd, 0x13)
}
fn slli(rd: u32, rs1: u32, sh: i32) -> u32 {
    i_type(sh, rs1, 1, rd, 0x13)
}
fn srli(rd: u32, rs1: u32, sh: i32) -> u32 {
    i_type(sh, rs1, 5, rd, 0x13)
}
fn add(rd: u32, rs1: u32, rs2: u32) -> u32 {
    r_type(0, rs2, rs1, 0, rd, 0x33)
}
fn sub(rd: u32, rs1: u32, rs2: u32) -> u32 {
    r_type(0x20, rs2, rs1, 0, rd, 0x33)
}
fn and(rd: u32, rs1: u32, rs2: u32) -> u32 {
    r_type(0, rs2, rs1, 7, rd, 0x33)
}
fn xor(rd: u32, rs1: u32, rs2: u32) -> u32 {
    r_type(0, rs2, rs1, 4, rd, 0x33)
}
fn mul(rd: u32, rs1: u32, rs2: u32) -> u32 {
    r_type(1, rs2, rs1, 0, rd, 0x33)
}
fn addw(rd: u32, rs1: u32, rs2: u32) -> u32 {
    r_type(0, rs2, rs1, 0, rd, 0x3b)
}
fn ld(rd: u32, rs1: u32, imm: i32) -> u32 {
    i_type(imm, rs1, 3, rd, 0x03)
}
fn lw(rd: u32, rs1: u32, imm: i32) -> u32 {
    i_type(imm, rs1, 2, rd, 0x03)
}
fn lbu(rd: u32, rs1: u32, imm: i32) -> u32 {
    i_type(imm, rs1, 4, rd, 0x03)
}
fn sd(rs1: u32, rs2: u32, imm: i32) -> u32 {
    s_type(imm, rs2, rs1, 3, 0x23)
}
fn sw(rs1: u32, rs2: u32, imm: i32) -> u32 {
    s_type(imm, rs2, rs1, 2, 0x23)
}
fn bne(rs1: u32, rs2: u32, imm: i32) -> u32 {
    b_type(imm, rs2, rs1, 1)
}

fn c_mv(rd: u32, rs2: u32) -> u16 {
    (0x8002 | (rd << 7) | (rs2 << 2)) as u16
}
fn c_add(rd: u32, rs2: u32) -> u16 {
    (0x9002 | (rd << 7) | (rs2 << 2)) as u16
}
fn c_li(rd: u32, imm: u32) -> u16 {
    (0x4001 | (rd << 7) | ((imm & 0x1f) << 2)) as u16
}
fn c_slli(rd: u32, sh: u32) -> u16 {
    (0x0002 | (rd << 7) | ((sh & 0x1f) << 2)) as u16
}

struct Asm {
    bytes: Vec<u8>,
}

impl Asm {
    fn new() -> Self {
        Asm { bytes: Vec::new() }
    }
    fn pos(&self) -> i32 {
        self.bytes.len() as i32
    }
    fn w(&mut self, insn: u32) {
        self.bytes.extend_from_slice(&insn.to_le_bytes());
    }
    fn h(&mut self, insn: u16) {
        self.bytes.extend_from_slice(&insn.to_le_bytes());
    }
    /// Materialize an arbitrary 64-bit constant, 11 bits at a time.
    fn li(&mut self, rd: u32, val: u64) {
        self.w(addi(rd, 0, 0));
        let mut shift: i32 = 64;
        while shift > 0 {
            let n = shift.min(11);
            shift -= n;
            self.w(slli(rd, rd, n));
            let chunk = ((val >> shift) & ((1u64 << n) - 1)) as i32;
            if chunk != 0 {
                self.w(addi(rd, rd, chunk));
            }
        }
    }
}

/// Synthetic guest: 40 copies of a 21-instruction mixed block wrapped in an
/// endless outer loop that sweeps the buffer 64 bytes at a time.
fn build_program() -> Vec<u8> {
    let mut a = Asm::new();
    // x31 = buffer base, x30 = wrap mask, x5 = running offset, x6 = counter.
    a.li(31, BUF);
    a.li(30, BUF_LEN - 64);
    a.w(addi(5, 0, 0));
    a.w(addi(6, 0, 0));

    let outer = a.pos();
    a.w(addi(5, 5, 64));
    a.w(and(5, 5, 30));
    a.w(add(4, 31, 5));

    for k in 0..40 {
        let off = (k % 4) * 8;
        a.w(ld(7, 4, off));
        a.w(ld(8, 4, off + 8));
        a.w(add(9, 7, 8));
        a.w(xor(10, 9, 6));
        a.w(slli(11, 10, 7));
        a.w(srli(12, 11, 3));
        a.w(add(13, 12, 9));
        a.w(sd(4, 13, off + 16));
        a.w(lw(14, 4, off + 24));
        a.w(addw(15, 14, 13));
        a.w(sw(4, 15, off + 24));
        a.w(mul(16, 15, 7));
        a.w(addi(17, 16, 17));
        a.w(sub(18, 17, 8));
        a.w(lbu(19, 4, off + 32));
        // Data-dependent branch, taken whenever the byte is non-zero.
        a.w(bne(19, 0, 8));
        a.w(addi(20, 0, 1));
        a.h(c_mv(21, 7));
        a.h(c_add(21, 8));
        a.h(c_slli(21, 1));
        a.h(c_li(22, 5));
    }

    a.w(addi(6, 6, 1));
    let back = outer - a.pos();
    a.w(j_type(back, 0));
    a.bytes
}

fn make_cpu(mmu: bool) -> Cpu {
    let ram = GuestRam::new_owned(RAM_BASE, RAM_SIZE);
    ram.write_slice(RAM_BASE, &build_program()).unwrap();
    // Non-zero buffer contents keep the data-dependent branch taken.
    for i in 0..BUF_LEN / 8 {
        ram.store(BUF + i * 8, 8, 0x0102_0304_0506_0709 ^ i)
            .unwrap();
    }

    let mut cpu = Cpu::new(0, ram.clone(), Clock::Deterministic { shift: 3 });
    if mmu {
        // Identity-map the low 4 GiB with Sv39 gigapages (V|R|W|X|A|D).
        for k in 0..4u64 {
            ram.store(PT_ROOT + k * 8, 8, ((k << 18) << 10) | 0xcf)
                .unwrap();
        }
        cpu.write_csr(0x180, (8 << 60) | (PT_ROOT >> 12)).unwrap();
    }
    cpu.priv_mode = PrivMode::S;
    cpu.set_pc(RAM_BASE);
    cpu
}

fn measure(name: &str, mmu: bool) {
    let mut cpu = make_cpu(mmu);
    // Warm the TLB and page tables before timing.
    cpu.run(200_000);
    let start_insns = cpu.instret();
    let t0 = Instant::now();
    let exit = cpu.run(INSNS);
    let elapsed = t0.elapsed();
    assert_eq!(exit, VmExit::InstrLimit);
    let executed = cpu.instret() - start_insns;
    let mips = executed as f64 / elapsed.as_secs_f64() / 1e6;
    println!(
        "bench {name:<4} {executed} insns in {:.3}s = {mips:.2} MIPS",
        elapsed.as_secs_f64()
    );
}

#[test]
#[ignore = "throughput benchmark; run explicitly with --release"]
fn mips_sv39() {
    measure("sv39", true);
}

#[test]
#[ignore = "throughput benchmark; run explicitly with --release"]
fn mips_bare() {
    measure("bare", false);
}
