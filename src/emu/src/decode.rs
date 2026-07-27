// Copyright 2026 The libkrun Authors. Licensed under Apache-2.0.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchOp {
    Eq,
    Ne,
    Lt,
    Ge,
    Ltu,
    Geu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadOp {
    Lb,
    Lh,
    Lw,
    Ld,
    Lbu,
    Lhu,
    Lwu,
}

impl LoadOp {
    pub fn size(self) -> usize {
        match self {
            LoadOp::Lb | LoadOp::Lbu => 1,
            LoadOp::Lh | LoadOp::Lhu => 2,
            LoadOp::Lw | LoadOp::Lwu => 4,
            LoadOp::Ld => 8,
        }
    }

    pub fn signed(self) -> bool {
        matches!(self, LoadOp::Lb | LoadOp::Lh | LoadOp::Lw)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreOp {
    Sb,
    Sh,
    Sw,
    Sd,
}

impl StoreOp {
    pub fn size(self) -> usize {
        match self {
            StoreOp::Sb => 1,
            StoreOp::Sh => 2,
            StoreOp::Sw => 4,
            StoreOp::Sd => 8,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AluOp {
    Add,
    Sub,
    Sll,
    Slt,
    Sltu,
    Xor,
    Srl,
    Sra,
    Or,
    And,
    // M extension (decoded here, executed once the M code lands).
    Mul,
    Mulh,
    Mulhsu,
    Mulhu,
    Div,
    Divu,
    Rem,
    Remu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CsrOp {
    Rw,
    Rs,
    Rc,
}

/// A extension: LR/SC plus the fetch-op AMOs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmoOp {
    Lr,
    Sc,
    Swap,
    Add,
    Xor,
    And,
    Or,
    Min,
    Max,
    Minu,
    Maxu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Instr {
    Lui {
        rd: u8,
        imm: i64,
    },
    Auipc {
        rd: u8,
        imm: i64,
    },
    Jal {
        rd: u8,
        imm: i64,
    },
    Jalr {
        rd: u8,
        rs1: u8,
        imm: i64,
    },
    Branch {
        op: BranchOp,
        rs1: u8,
        rs2: u8,
        imm: i64,
    },
    Load {
        op: LoadOp,
        rd: u8,
        rs1: u8,
        imm: i64,
    },
    Store {
        op: StoreOp,
        rs1: u8,
        rs2: u8,
        imm: i64,
    },
    OpImm {
        op: AluOp,
        rd: u8,
        rs1: u8,
        imm: i64,
    },
    OpImm32 {
        op: AluOp,
        rd: u8,
        rs1: u8,
        imm: i64,
    },
    Op {
        op: AluOp,
        rd: u8,
        rs1: u8,
        rs2: u8,
    },
    Op32 {
        op: AluOp,
        rd: u8,
        rs1: u8,
        rs2: u8,
    },
    Fence,
    FenceI,
    Ecall,
    Ebreak,
    /// `imm` selects the zimm (immediate) form; `rs1` then holds zimm.
    Csr {
        op: CsrOp,
        rd: u8,
        rs1: u8,
        csr: u16,
        imm: bool,
    },
    Mret,
    Sret,
    Wfi,
    SfenceVma,
    /// A extension. `width` is 4 (`.w`) or 8 (`.d`). `aq`/`rl` are decoded
    /// but unused: the interpreter is sequentially consistent regardless.
    Amo {
        op: AmoOp,
        rd: u8,
        rs1: u8,
        rs2: u8,
        width: u8,
        aq: bool,
        rl: bool,
    },
    /// F/D extensions, decoded coarsely until the FP code lands.
    Fp {
        raw: u32,
    },
    Illegal(u32),
}

#[inline]
fn rd(i: u32) -> u8 {
    ((i >> 7) & 0x1f) as u8
}

#[inline]
fn rs1(i: u32) -> u8 {
    ((i >> 15) & 0x1f) as u8
}

#[inline]
fn rs2(i: u32) -> u8 {
    ((i >> 20) & 0x1f) as u8
}

#[inline]
fn funct3(i: u32) -> u32 {
    (i >> 12) & 7
}

#[inline]
fn imm_i(i: u32) -> i64 {
    ((i as i32) >> 20) as i64
}

#[inline]
fn imm_s(i: u32) -> i64 {
    ((((i as i32) >> 25) as i64) << 5) | ((i >> 7) & 0x1f) as i64
}

#[inline]
fn imm_b(i: u32) -> i64 {
    ((((i as i32) >> 31) as i64) << 12)
        | (((i >> 25) & 0x3f) as i64) << 5
        | (((i >> 8) & 0xf) as i64) << 1
        | (((i >> 7) & 1) as i64) << 11
}

#[inline]
fn imm_u(i: u32) -> i64 {
    (i & 0xffff_f000) as i32 as i64
}

#[inline]
fn imm_j(i: u32) -> i64 {
    ((((i as i32) >> 31) as i64) << 20)
        | (((i >> 21) & 0x3ff) as i64) << 1
        | (((i >> 20) & 1) as i64) << 11
        | (i & 0xf_f000) as i64
}

/// Decode a 32-bit instruction.
pub fn decode(i: u32) -> Instr {
    let op = i & 0x7f;
    match op {
        0x37 => Instr::Lui {
            rd: rd(i),
            imm: imm_u(i),
        },
        0x17 => Instr::Auipc {
            rd: rd(i),
            imm: imm_u(i),
        },
        0x6f => Instr::Jal {
            rd: rd(i),
            imm: imm_j(i),
        },
        0x67 if funct3(i) == 0 => Instr::Jalr {
            rd: rd(i),
            rs1: rs1(i),
            imm: imm_i(i),
        },
        0x63 => {
            let bop = match funct3(i) {
                0 => BranchOp::Eq,
                1 => BranchOp::Ne,
                4 => BranchOp::Lt,
                5 => BranchOp::Ge,
                6 => BranchOp::Ltu,
                7 => BranchOp::Geu,
                _ => return Instr::Illegal(i),
            };
            Instr::Branch {
                op: bop,
                rs1: rs1(i),
                rs2: rs2(i),
                imm: imm_b(i),
            }
        }
        0x03 => {
            let lop = match funct3(i) {
                0 => LoadOp::Lb,
                1 => LoadOp::Lh,
                2 => LoadOp::Lw,
                3 => LoadOp::Ld,
                4 => LoadOp::Lbu,
                5 => LoadOp::Lhu,
                6 => LoadOp::Lwu,
                _ => return Instr::Illegal(i),
            };
            Instr::Load {
                op: lop,
                rd: rd(i),
                rs1: rs1(i),
                imm: imm_i(i),
            }
        }
        0x23 => {
            let sop = match funct3(i) {
                0 => StoreOp::Sb,
                1 => StoreOp::Sh,
                2 => StoreOp::Sw,
                3 => StoreOp::Sd,
                _ => return Instr::Illegal(i),
            };
            Instr::Store {
                op: sop,
                rs1: rs1(i),
                rs2: rs2(i),
                imm: imm_s(i),
            }
        }
        0x13 => {
            // RV64 shifts take a 6-bit shamt; bits 31:26 select the op.
            let aop = match funct3(i) {
                0 => AluOp::Add,
                1 if i >> 26 == 0 => AluOp::Sll,
                2 => AluOp::Slt,
                3 => AluOp::Sltu,
                4 => AluOp::Xor,
                5 if i >> 26 == 0 => AluOp::Srl,
                5 if i >> 26 == 0x10 => AluOp::Sra,
                6 => AluOp::Or,
                7 => AluOp::And,
                _ => return Instr::Illegal(i),
            };
            let imm = match aop {
                AluOp::Sll | AluOp::Srl | AluOp::Sra => ((i >> 20) & 0x3f) as i64,
                _ => imm_i(i),
            };
            Instr::OpImm {
                op: aop,
                rd: rd(i),
                rs1: rs1(i),
                imm,
            }
        }
        0x1b => {
            let aop = match funct3(i) {
                0 => AluOp::Add,
                1 if i >> 25 == 0 => AluOp::Sll,
                5 if i >> 25 == 0 => AluOp::Srl,
                5 if i >> 25 == 0x20 => AluOp::Sra,
                _ => return Instr::Illegal(i),
            };
            let imm = match aop {
                AluOp::Sll | AluOp::Srl | AluOp::Sra => ((i >> 20) & 0x1f) as i64,
                _ => imm_i(i),
            };
            Instr::OpImm32 {
                op: aop,
                rd: rd(i),
                rs1: rs1(i),
                imm,
            }
        }
        0x33 => {
            let aop = match (i >> 25, funct3(i)) {
                (0x00, 0) => AluOp::Add,
                (0x00, 1) => AluOp::Sll,
                (0x00, 2) => AluOp::Slt,
                (0x00, 3) => AluOp::Sltu,
                (0x00, 4) => AluOp::Xor,
                (0x00, 5) => AluOp::Srl,
                (0x00, 6) => AluOp::Or,
                (0x00, 7) => AluOp::And,
                (0x20, 0) => AluOp::Sub,
                (0x20, 5) => AluOp::Sra,
                (0x01, 0) => AluOp::Mul,
                (0x01, 1) => AluOp::Mulh,
                (0x01, 2) => AluOp::Mulhsu,
                (0x01, 3) => AluOp::Mulhu,
                (0x01, 4) => AluOp::Div,
                (0x01, 5) => AluOp::Divu,
                (0x01, 6) => AluOp::Rem,
                (0x01, 7) => AluOp::Remu,
                _ => return Instr::Illegal(i),
            };
            Instr::Op {
                op: aop,
                rd: rd(i),
                rs1: rs1(i),
                rs2: rs2(i),
            }
        }
        0x3b => {
            let aop = match (i >> 25, funct3(i)) {
                (0x00, 0) => AluOp::Add,
                (0x00, 1) => AluOp::Sll,
                (0x00, 5) => AluOp::Srl,
                (0x20, 0) => AluOp::Sub,
                (0x20, 5) => AluOp::Sra,
                (0x01, 0) => AluOp::Mul,
                (0x01, 4) => AluOp::Div,
                (0x01, 5) => AluOp::Divu,
                (0x01, 6) => AluOp::Rem,
                (0x01, 7) => AluOp::Remu,
                _ => return Instr::Illegal(i),
            };
            Instr::Op32 {
                op: aop,
                rd: rd(i),
                rs1: rs1(i),
                rs2: rs2(i),
            }
        }
        0x0f => match funct3(i) {
            0 => Instr::Fence,
            1 => Instr::FenceI,
            _ => Instr::Illegal(i),
        },
        0x73 => {
            let f3 = funct3(i);
            match f3 {
                0 => match (i >> 25, rs2(i), rs1(i), rd(i)) {
                    (0x00, 0, 0, 0) => Instr::Ecall,
                    (0x00, 1, 0, 0) => Instr::Ebreak,
                    (0x08, 2, 0, 0) => Instr::Sret,
                    (0x18, 2, 0, 0) => Instr::Mret,
                    (0x08, 5, 0, 0) => Instr::Wfi,
                    (0x09, _, _, 0) => Instr::SfenceVma,
                    _ => Instr::Illegal(i),
                },
                1..=3 | 5..=7 => {
                    let cop = match f3 & 3 {
                        1 => CsrOp::Rw,
                        2 => CsrOp::Rs,
                        _ => CsrOp::Rc,
                    };
                    Instr::Csr {
                        op: cop,
                        rd: rd(i),
                        rs1: rs1(i),
                        csr: (i >> 20) as u16,
                        imm: f3 >= 5,
                    }
                }
                _ => Instr::Illegal(i),
            }
        }
        0x2f => {
            let width = match funct3(i) {
                2 => 4,
                3 => 8,
                _ => return Instr::Illegal(i),
            };
            let aop = match i >> 27 {
                0x00 => AmoOp::Add,
                0x01 => AmoOp::Swap,
                0x02 if rs2(i) == 0 => AmoOp::Lr,
                0x03 => AmoOp::Sc,
                0x04 => AmoOp::Xor,
                0x08 => AmoOp::Or,
                0x0c => AmoOp::And,
                0x10 => AmoOp::Min,
                0x14 => AmoOp::Max,
                0x18 => AmoOp::Minu,
                0x1c => AmoOp::Maxu,
                _ => return Instr::Illegal(i),
            };
            Instr::Amo {
                op: aop,
                rd: rd(i),
                rs1: rs1(i),
                rs2: rs2(i),
                width,
                aq: i & (1 << 26) != 0,
                rl: i & (1 << 25) != 0,
            }
        }
        0x07 | 0x27 | 0x43 | 0x47 | 0x4b | 0x4f | 0x53 => Instr::Fp { raw: i },
        _ => Instr::Illegal(i),
    }
}

/// Sign-extend the low `bits` of `v`.
#[inline]
fn sext(v: u32, bits: u32) -> i64 {
    ((v as i64) << (64 - bits)) >> (64 - bits)
}

/// c.lw/c.sw offset: uimm[5:3] at 12:10, uimm[2] at 6, uimm[6] at 5.
#[inline]
fn cimm_w(i: u32) -> i64 {
    (((i >> 10) & 7) << 3 | ((i >> 6) & 1) << 2 | ((i >> 5) & 1) << 6) as i64
}

/// c.ld/c.sd/c.fld/c.fsd offset: uimm[5:3] at 12:10, uimm[7:6] at 6:5.
#[inline]
fn cimm_d(i: u32) -> i64 {
    (((i >> 10) & 7) << 3 | ((i >> 5) & 3) << 6) as i64
}

/// Sign-extended 6-bit immediate: imm[5] at 12, imm[4:0] at 6:2.
#[inline]
fn cimm6(i: u32) -> i64 {
    sext(((i >> 12) & 1) << 5 | ((i >> 2) & 0x1f), 6)
}

/// RV64 shift amount: shamt[5] at 12, shamt[4:0] at 6:2.
#[inline]
fn cshamt(i: u32) -> i64 {
    (((i >> 12) & 1) << 5 | ((i >> 2) & 0x1f)) as i64
}

/// c.j target: imm[11|4|9:8|10|6|7|3:1|5] at 12:2.
#[inline]
fn cimm_j(i: u32) -> i64 {
    sext(
        ((i >> 12) & 1) << 11
            | ((i >> 11) & 1) << 4
            | ((i >> 9) & 3) << 8
            | ((i >> 8) & 1) << 10
            | ((i >> 7) & 1) << 6
            | ((i >> 6) & 1) << 7
            | ((i >> 3) & 7) << 1
            | ((i >> 2) & 1) << 5,
        12,
    )
}

/// c.beqz/c.bnez target: imm[8|4:3] at 12:10, imm[7:6|2:1|5] at 6:2.
#[inline]
fn cimm_b(i: u32) -> i64 {
    sext(
        ((i >> 12) & 1) << 8
            | ((i >> 10) & 3) << 3
            | ((i >> 5) & 3) << 6
            | ((i >> 3) & 3) << 1
            | ((i >> 2) & 1) << 5,
        9,
    )
}

/// c.lwsp offset: uimm[5] at 12, uimm[4:2] at 6:4, uimm[7:6] at 3:2.
#[inline]
fn cimm_lwsp(i: u32) -> i64 {
    (((i >> 12) & 1) << 5 | ((i >> 4) & 7) << 2 | ((i >> 2) & 3) << 6) as i64
}

/// c.ldsp/c.fldsp offset: uimm[5] at 12, uimm[4:3] at 6:5, uimm[8:6] at 4:2.
#[inline]
fn cimm_ldsp(i: u32) -> i64 {
    (((i >> 12) & 1) << 5 | ((i >> 5) & 3) << 3 | ((i >> 2) & 7) << 6) as i64
}

/// c.swsp offset: uimm[5:2] at 12:9, uimm[7:6] at 8:7.
#[inline]
fn cimm_swsp(i: u32) -> i64 {
    (((i >> 9) & 0xf) << 2 | ((i >> 7) & 3) << 6) as i64
}

/// c.sdsp/c.fsdsp offset: uimm[5:3] at 12:10, uimm[8:6] at 9:7.
#[inline]
fn cimm_sdsp(i: u32) -> i64 {
    (((i >> 10) & 7) << 3 | ((i >> 7) & 7) << 6) as i64
}

/// Full-width `fld` word — the expansion target for c.fld/c.fldsp.
#[inline]
fn fld_word(frd: u8, rs1: u8, uimm: i64) -> u32 {
    ((uimm as u32) << 20) | ((rs1 as u32) << 15) | (3 << 12) | ((frd as u32) << 7) | 0x07
}

/// Full-width `fsd` word — the expansion target for c.fsd/c.fsdsp.
#[inline]
fn fsd_word(rs1: u8, frs2: u8, uimm: i64) -> u32 {
    let u = uimm as u32;
    ((u >> 5) << 25)
        | ((frs2 as u32) << 20)
        | ((rs1 as u32) << 15)
        | (3 << 12)
        | ((u & 0x1f) << 7)
        | 0x27
}

/// Expand a compressed (C extension) instruction: the RV64 subset of RVC,
/// quadrants 0-2. Reserved encodings decode to `Illegal` carrying the
/// 16-bit bits zero-extended (the xtval contract for illegal compressed
/// instructions). The c.fld/c.fsd family expands to full fld/fsd words
/// inside `Instr::Fp`, so decode stays complete whether or not the FP
/// execute path has landed.
pub fn decode_compressed(raw: u16) -> Instr {
    let i = raw as u32;
    let ill = Instr::Illegal(i);
    // x8..x15 fields (rd'/rs1' at 9:7, rd'/rs2' at 4:2) and full fields.
    let r97 = 8 + ((i >> 7) & 7) as u8;
    let r42 = 8 + ((i >> 2) & 7) as u8;
    let rd = ((i >> 7) & 0x1f) as u8;
    let rs2 = ((i >> 2) & 0x1f) as u8;

    match (i & 3, (i >> 13) & 7) {
        (0, 0) => {
            // c.addi4spn: nzuimm[5:4|9:6|2|3] at 12:5; nzuimm == 0 (which
            // includes the all-zero halfword) is reserved.
            let uimm = ((i >> 11) & 3) << 4
                | ((i >> 7) & 0xf) << 6
                | ((i >> 6) & 1) << 2
                | ((i >> 5) & 1) << 3;
            if uimm == 0 {
                return ill;
            }
            Instr::OpImm {
                op: AluOp::Add,
                rd: r42,
                rs1: 2,
                imm: uimm as i64,
            }
        }
        (0, 1) => Instr::Fp {
            raw: fld_word(r42, r97, cimm_d(i)), // c.fld
        },
        (0, 2) => Instr::Load {
            op: LoadOp::Lw,
            rd: r42,
            rs1: r97,
            imm: cimm_w(i),
        },
        (0, 3) => Instr::Load {
            op: LoadOp::Ld,
            rd: r42,
            rs1: r97,
            imm: cimm_d(i),
        },
        (0, 5) => Instr::Fp {
            raw: fsd_word(r97, r42, cimm_d(i)), // c.fsd
        },
        (0, 6) => Instr::Store {
            op: StoreOp::Sw,
            rs1: r97,
            rs2: r42,
            imm: cimm_w(i),
        },
        (0, 7) => Instr::Store {
            op: StoreOp::Sd,
            rs1: r97,
            rs2: r42,
            imm: cimm_d(i),
        },
        // c.addi (rd = x0 is c.nop; other hints execute as plain addi).
        (1, 0) => Instr::OpImm {
            op: AluOp::Add,
            rd,
            rs1: rd,
            imm: cimm6(i),
        },
        // c.addiw; rd = x0 is reserved (falls to the final arm).
        (1, 1) if rd != 0 => Instr::OpImm32 {
            op: AluOp::Add,
            rd,
            rs1: rd,
            imm: cimm6(i),
        },
        (1, 2) => Instr::OpImm {
            op: AluOp::Add,
            rd,
            rs1: 0,
            imm: cimm6(i), // c.li
        },
        (1, 3) if rd == 2 => {
            // c.addi16sp: nzimm[9|4|6|8:7|5] at 12, 6:2; zero is reserved.
            let imm = sext(
                ((i >> 12) & 1) << 9
                    | ((i >> 6) & 1) << 4
                    | ((i >> 5) & 1) << 6
                    | ((i >> 3) & 3) << 7
                    | ((i >> 2) & 1) << 5,
                10,
            );
            if imm == 0 {
                return ill;
            }
            Instr::OpImm {
                op: AluOp::Add,
                rd: 2,
                rs1: 2,
                imm,
            }
        }
        (1, 3) => {
            // c.lui: nzimm[17|16:12]; zero is reserved (rd = x0 is a HINT).
            let imm = sext(((i >> 12) & 1) << 17 | ((i >> 2) & 0x1f) << 12, 18);
            if imm == 0 {
                return ill;
            }
            Instr::Lui { rd, imm }
        }
        (1, 4) => match (i >> 10) & 3 {
            0 => Instr::OpImm {
                op: AluOp::Srl,
                rd: r97,
                rs1: r97,
                imm: cshamt(i), // c.srli
            },
            1 => Instr::OpImm {
                op: AluOp::Sra,
                rd: r97,
                rs1: r97,
                imm: cshamt(i), // c.srai
            },
            2 => Instr::OpImm {
                op: AluOp::And,
                rd: r97,
                rs1: r97,
                imm: cimm6(i), // c.andi
            },
            _ => {
                let aop = match ((i >> 12) & 1, (i >> 5) & 3) {
                    (0, 0) => AluOp::Sub,
                    (0, 1) => AluOp::Xor,
                    (0, 2) => AluOp::Or,
                    (0, 3) => AluOp::And,
                    (1, 0) => AluOp::Sub, // c.subw
                    (1, 1) => AluOp::Add, // c.addw
                    _ => return ill,      // reserved
                };
                if (i >> 12) & 1 == 0 {
                    Instr::Op {
                        op: aop,
                        rd: r97,
                        rs1: r97,
                        rs2: r42,
                    }
                } else {
                    Instr::Op32 {
                        op: aop,
                        rd: r97,
                        rs1: r97,
                        rs2: r42,
                    }
                }
            }
        },
        (1, 5) => Instr::Jal {
            rd: 0,
            imm: cimm_j(i), // c.j
        },
        (1, 6) => Instr::Branch {
            op: BranchOp::Eq,
            rs1: r97,
            rs2: 0,
            imm: cimm_b(i), // c.beqz
        },
        (1, 7) => Instr::Branch {
            op: BranchOp::Ne,
            rs1: r97,
            rs2: 0,
            imm: cimm_b(i), // c.bnez
        },
        (2, 0) => Instr::OpImm {
            op: AluOp::Sll,
            rd,
            rs1: rd,
            imm: cshamt(i), // c.slli
        },
        (2, 1) => Instr::Fp {
            raw: fld_word(rd, 2, cimm_ldsp(i)), // c.fldsp
        },
        // c.lwsp/c.ldsp; rd = x0 is reserved (falls to the final arm).
        (2, 2) if rd != 0 => Instr::Load {
            op: LoadOp::Lw,
            rd,
            rs1: 2,
            imm: cimm_lwsp(i),
        },
        (2, 3) if rd != 0 => Instr::Load {
            op: LoadOp::Ld,
            rd,
            rs1: 2,
            imm: cimm_ldsp(i),
        },
        (2, 4) => match ((i >> 12) & 1, rs2, rd) {
            (0, 0, 0) => ill, // reserved
            (0, 0, _) => Instr::Jalr {
                rd: 0,
                rs1: rd,
                imm: 0, // c.jr
            },
            (0, _, _) => Instr::Op {
                op: AluOp::Add,
                rd,
                rs1: 0,
                rs2, // c.mv
            },
            (1, 0, 0) => Instr::Ebreak,
            (1, 0, _) => Instr::Jalr {
                rd: 1,
                rs1: rd,
                imm: 0, // c.jalr
            },
            _ => Instr::Op {
                op: AluOp::Add,
                rd,
                rs1: rd,
                rs2, // c.add
            },
        },
        (2, 5) => Instr::Fp {
            raw: fsd_word(2, rs2, cimm_sdsp(i)), // c.fsdsp
        },
        (2, 6) => Instr::Store {
            op: StoreOp::Sw,
            rs1: 2,
            rs2,
            imm: cimm_swsp(i), // c.swsp
        },
        (2, 7) => Instr::Store {
            op: StoreOp::Sd,
            rs1: 2,
            rs2,
            imm: cimm_sdsp(i), // c.sdsp
        },
        // Quadrant 0 funct3 4 is reserved; quadrant 3 is not compressed.
        _ => ill,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Golden encodings cross-checked against riscv64-unknown-elf-as.
    #[test]
    fn golden_rv64i() {
        // addi x1, x2, -5
        assert_eq!(
            decode(0xffb1_0093),
            Instr::OpImm {
                op: AluOp::Add,
                rd: 1,
                rs1: 2,
                imm: -5
            }
        );
        // lui x5, 0x10000
        assert_eq!(
            decode(0x1000_02b7),
            Instr::Lui {
                rd: 5,
                imm: 0x1000_0000
            }
        );
        // auipc x3, 0xfffff
        assert_eq!(decode(0xffff_f197), Instr::Auipc { rd: 3, imm: -4096 });
        // jal x1, +16
        assert_eq!(decode(0x0100_00ef), Instr::Jal { rd: 1, imm: 16 });
        // jal x0, -8
        assert_eq!(decode(0xff9f_f06f), Instr::Jal { rd: 0, imm: -8 });
        // jalr x0, 0(x1)
        assert_eq!(
            decode(0x0000_8067),
            Instr::Jalr {
                rd: 0,
                rs1: 1,
                imm: 0
            }
        );
        // beq x1, x2, -4
        assert_eq!(
            decode(0xfe20_8ee3),
            Instr::Branch {
                op: BranchOp::Eq,
                rs1: 1,
                rs2: 2,
                imm: -4
            }
        );
        // bltu x10, x11, +8
        assert_eq!(
            decode(0x00b5_6463),
            Instr::Branch {
                op: BranchOp::Ltu,
                rs1: 10,
                rs2: 11,
                imm: 8
            }
        );
        // lw x6, 0(x5)
        assert_eq!(
            decode(0x0002_a303),
            Instr::Load {
                op: LoadOp::Lw,
                rd: 6,
                rs1: 5,
                imm: 0
            }
        );
        // lbu x7, -1(x8)
        assert_eq!(
            decode(0xfff4_4383),
            Instr::Load {
                op: LoadOp::Lbu,
                rd: 7,
                rs1: 8,
                imm: -1
            }
        );
        // sd x6, 8(x5)
        assert_eq!(
            decode(0x0062_b423),
            Instr::Store {
                op: StoreOp::Sd,
                rs1: 5,
                rs2: 6,
                imm: 8
            }
        );
        // slli x1, x1, 63
        assert_eq!(
            decode(0x03f0_9093),
            Instr::OpImm {
                op: AluOp::Sll,
                rd: 1,
                rs1: 1,
                imm: 63
            }
        );
        // srai x2, x2, 1
        assert_eq!(
            decode(0x4011_5113),
            Instr::OpImm {
                op: AluOp::Sra,
                rd: 2,
                rs1: 2,
                imm: 1
            }
        );
        // sraiw x2, x3, 31
        assert_eq!(
            decode(0x41f1_d11b),
            Instr::OpImm32 {
                op: AluOp::Sra,
                rd: 2,
                rs1: 3,
                imm: 31
            }
        );
        // sub x4, x5, x6
        assert_eq!(
            decode(0x4062_8233),
            Instr::Op {
                op: AluOp::Sub,
                rd: 4,
                rs1: 5,
                rs2: 6
            }
        );
        // addw x4, x5, x6
        assert_eq!(
            decode(0x0062_823b),
            Instr::Op32 {
                op: AluOp::Add,
                rd: 4,
                rs1: 5,
                rs2: 6
            }
        );
        // mul x4, x5, x6 (M: decoded, not yet executed)
        assert_eq!(
            decode(0x0262_8233),
            Instr::Op {
                op: AluOp::Mul,
                rd: 4,
                rs1: 5,
                rs2: 6
            }
        );
    }

    #[test]
    fn golden_system() {
        assert_eq!(decode(0x0000_0073), Instr::Ecall);
        assert_eq!(decode(0x0010_0073), Instr::Ebreak);
        assert_eq!(decode(0x3020_0073), Instr::Mret);
        assert_eq!(decode(0x1020_0073), Instr::Sret);
        assert_eq!(decode(0x1050_0073), Instr::Wfi);
        assert_eq!(decode(0x1200_0073), Instr::SfenceVma);
        assert_eq!(decode(0x0000_100f), Instr::FenceI);
        assert_eq!(decode(0x0ff0_000f), Instr::Fence);
        // csrrwi x0, mscratch, 5
        assert_eq!(
            decode(0x3402_d073),
            Instr::Csr {
                op: CsrOp::Rw,
                rd: 0,
                rs1: 5,
                csr: 0x340,
                imm: true
            }
        );
        // csrrs x7, mscratch, x0
        assert_eq!(
            decode(0x3400_23f3),
            Instr::Csr {
                op: CsrOp::Rs,
                rd: 7,
                rs1: 0,
                csr: 0x340,
                imm: false
            }
        );
    }

    #[test]
    fn illegal_and_stubs() {
        assert_eq!(decode(0x0000_0000), Instr::Illegal(0));
        // slli with bit 26 garbage is illegal
        assert_eq!(decode(0x0800_9093), Instr::Illegal(0x0800_9093));
        // fld decodes to the FP stub
        assert!(matches!(decode(0x0001_3007), Instr::Fp { .. }));
    }

    #[test]
    fn golden_amo() {
        // amoswap.w x4, x6, (x5)
        assert_eq!(
            decode(0x0862_a22f),
            Instr::Amo {
                op: AmoOp::Swap,
                rd: 4,
                rs1: 5,
                rs2: 6,
                width: 4,
                aq: false,
                rl: false
            }
        );
        // lr.d x6, (x5) with aq
        assert_eq!(
            decode(0x1402_b32f),
            Instr::Amo {
                op: AmoOp::Lr,
                rd: 6,
                rs1: 5,
                rs2: 0,
                width: 8,
                aq: true,
                rl: false
            }
        );
        // sc.w x7, x6, (x5) with rl
        assert_eq!(
            decode(0x1a62_a3af),
            Instr::Amo {
                op: AmoOp::Sc,
                rd: 7,
                rs1: 5,
                rs2: 6,
                width: 4,
                aq: false,
                rl: true
            }
        );
        // amomaxu.d x8, x6, (x5)
        assert_eq!(
            decode(0xe062_b42f),
            Instr::Amo {
                op: AmoOp::Maxu,
                rd: 8,
                rs1: 5,
                rs2: 6,
                width: 8,
                aq: false,
                rl: false
            }
        );
        // lr with rs2 != 0 is illegal
        assert_eq!(decode(0x1012_a32f), Instr::Illegal(0x1012_a32f));
        // amo with a byte funct3 is illegal
        assert_eq!(decode(0x0862_822f), Instr::Illegal(0x0862_822f));
    }

    // Compressed golden cases cross-checked against the objdump disassembly
    // of riscv-tests rv64uc-p-rvc (rv64uc-p-rvc.dump).
    #[test]
    fn golden_rvc() {
        use Instr::*;
        // 1fe8: addi a0,sp,1020 (c.addi4spn)
        assert_eq!(
            decode_compressed(0x1fe8),
            OpImm {
                op: AluOp::Add,
                rd: 10,
                rs1: 2,
                imm: 1020
            }
        );
        // 1541: addi a0,a0,-16 (c.addi)
        assert_eq!(
            decode_compressed(0x1541),
            OpImm {
                op: AluOp::Add,
                rd: 10,
                rs1: 10,
                imm: -16
            }
        );
        // 7101: addi sp,sp,-512 and 617d: addi sp,sp,496 (c.addi16sp)
        assert_eq!(
            decode_compressed(0x7101),
            OpImm {
                op: AluOp::Add,
                rd: 2,
                rs1: 2,
                imm: -512
            }
        );
        assert_eq!(
            decode_compressed(0x617d),
            OpImm {
                op: AluOp::Add,
                rd: 2,
                rs1: 2,
                imm: 496
            }
        );
        // 357d: addiw a0,a0,-1 (c.addiw)
        assert_eq!(
            decode_compressed(0x357d),
            OpImm32 {
                op: AluOp::Add,
                rd: 10,
                rs1: 10,
                imm: -1
            }
        );
        // 557d: li a0,-1 (c.li)
        assert_eq!(
            decode_compressed(0x557d),
            OpImm {
                op: AluOp::Add,
                rd: 10,
                rs1: 0,
                imm: -1
            }
        );
        // 6405: lui s0,0x1 and 7405: lui s0,0xfffe1 (c.lui)
        assert_eq!(decode_compressed(0x6405), Lui { rd: 8, imm: 0x1000 });
        assert_eq!(
            decode_compressed(0x7405),
            Lui {
                rd: 8,
                imm: -0x1f000
            }
        );
        // 8031: srli s0,s0,0xc / 8431: srai / 983d: andi s0,s0,-17 / 0412: slli
        assert_eq!(
            decode_compressed(0x8031),
            OpImm {
                op: AluOp::Srl,
                rd: 8,
                rs1: 8,
                imm: 12
            }
        );
        assert_eq!(
            decode_compressed(0x8431),
            OpImm {
                op: AluOp::Sra,
                rd: 8,
                rs1: 8,
                imm: 12
            }
        );
        assert_eq!(
            decode_compressed(0x983d),
            OpImm {
                op: AluOp::And,
                rd: 8,
                rs1: 8,
                imm: -17
            }
        );
        assert_eq!(
            decode_compressed(0x0412),
            OpImm {
                op: AluOp::Sll,
                rd: 8,
                rs1: 8,
                imm: 4
            }
        );
        // 8c89/8ca9/8cc9/8ce9: sub/xor/or/and s1,s1,a0; 9c89/9ca9: subw/addw
        for (bits, aop) in [
            (0x8c89, AluOp::Sub),
            (0x8ca9, AluOp::Xor),
            (0x8cc9, AluOp::Or),
            (0x8ce9, AluOp::And),
        ] {
            assert_eq!(
                decode_compressed(bits),
                Op {
                    op: aop,
                    rd: 9,
                    rs1: 9,
                    rs2: 10
                }
            );
        }
        for (bits, aop) in [(0x9c89, AluOp::Sub), (0x9ca9, AluOp::Add)] {
            assert_eq!(
                decode_compressed(bits),
                Op32 {
                    op: aop,
                    rd: 9,
                    rs1: 9,
                    rs2: 10
                }
            );
        }
        // a011: j +4 (c.j); c111/e111: beqz/bnez a0,+4
        assert_eq!(decode_compressed(0xa011), Jal { rd: 0, imm: 4 });
        assert_eq!(
            decode_compressed(0xc111),
            Branch {
                op: BranchOp::Eq,
                rs1: 10,
                rs2: 0,
                imm: 4
            }
        );
        assert_eq!(
            decode_compressed(0xe111),
            Branch {
                op: BranchOp::Ne,
                rs1: 10,
                rs2: 0,
                imm: 4
            }
        );
        // 8282: jr t0; 9282: jalr t0; 82aa: mv t0,a0; 92aa: add t0,t0,a0
        assert_eq!(
            decode_compressed(0x8282),
            Jalr {
                rd: 0,
                rs1: 5,
                imm: 0
            }
        );
        assert_eq!(
            decode_compressed(0x9282),
            Jalr {
                rd: 1,
                rs1: 5,
                imm: 0
            }
        );
        assert_eq!(
            decode_compressed(0x82aa),
            Op {
                op: AluOp::Add,
                rd: 5,
                rs1: 0,
                rs2: 10
            }
        );
        assert_eq!(
            decode_compressed(0x92aa),
            Op {
                op: AluOp::Add,
                rd: 5,
                rs1: 5,
                rs2: 10
            }
        );
        // 41c8: lw a0,4(a1); 6188: ld a0,0(a1); c1c8: sw; e188: sd
        assert_eq!(
            decode_compressed(0x41c8),
            Load {
                op: LoadOp::Lw,
                rd: 10,
                rs1: 11,
                imm: 4
            }
        );
        assert_eq!(
            decode_compressed(0x6188),
            Load {
                op: LoadOp::Ld,
                rd: 10,
                rs1: 11,
                imm: 0
            }
        );
        assert_eq!(
            decode_compressed(0xc1c8),
            Store {
                op: StoreOp::Sw,
                rs1: 11,
                rs2: 10,
                imm: 4
            }
        );
        assert_eq!(
            decode_compressed(0xe188),
            Store {
                op: StoreOp::Sd,
                rs1: 11,
                rs2: 10,
                imm: 0
            }
        );
        // 4532: lw a0,12(sp); 6522: ld a0,8(sp); c62a: sw a0,12(sp);
        // e42a: sd a0,8(sp)
        assert_eq!(
            decode_compressed(0x4532),
            Load {
                op: LoadOp::Lw,
                rd: 10,
                rs1: 2,
                imm: 12
            }
        );
        assert_eq!(
            decode_compressed(0x6522),
            Load {
                op: LoadOp::Ld,
                rd: 10,
                rs1: 2,
                imm: 8
            }
        );
        assert_eq!(
            decode_compressed(0xc62a),
            Store {
                op: StoreOp::Sw,
                rs1: 2,
                rs2: 10,
                imm: 12
            }
        );
        assert_eq!(
            decode_compressed(0xe42a),
            Store {
                op: StoreOp::Sd,
                rs1: 2,
                rs2: 10,
                imm: 8
            }
        );
        // 0001: c.nop; 9002: c.ebreak
        assert_eq!(
            decode_compressed(0x0001),
            OpImm {
                op: AluOp::Add,
                rd: 0,
                rs1: 0,
                imm: 0
            }
        );
        assert_eq!(decode_compressed(0x9002), Ebreak);
        // 2188: c.fld fa0,0(a1) and a022: c.fsdsp fs0,0(sp) expand to the
        // full fld/fsd words.
        assert_eq!(decode_compressed(0x2188), Fp { raw: 0x0005_b507 });
        assert_eq!(decode_compressed(0xa022), Fp { raw: 0x0081_3027 });
    }

    #[test]
    fn rvc_reserved_encodings() {
        for bits in [
            0x0000u16, // all-zero halfword (c.addi4spn with nzuimm = 0)
            0x8000,    // quadrant 0 funct3 = 100
            0x2001,    // c.addiw with rd = x0
            0x6001,    // c.lui with nzimm = 0
            0x6101,    // c.addi16sp with nzimm = 0
            0x9c41,    // quadrant 1 op group, bit 12 = 1, sel = 2
            0x4002,    // c.lwsp with rd = x0
            0x6002,    // c.ldsp with rd = x0
            0x8002,    // c.jr with rs1 = x0
        ] {
            assert_eq!(decode_compressed(bits), Instr::Illegal(bits as u32));
        }
    }
}
