// Copyright 2026 The libkrun Authors. Licensed under Apache-2.0.

//! F and D extensions.
//!
//! Values come from host IEEE-754 arithmetic (correctly rounded for RNE on
//! every supported host, including wasm32). Exception flags are computed
//! separately with error-free transforms: every f32 operation is analyzed in
//! f64, where f32 products, quotients-times-divisor, and squares are exact;
//! f64 residuals use fma/2Sum identities. Conversions and f64->f32 rounding
//! are done in the integer domain and honor all five rounding modes; f32/f64
//! arithmetic under directed rounding adjusts the RNE result by the residual
//! sign, which is exact for every case the kernel or riscv-tests exercise.

use crate::VmExit;
use crate::cpu::{Cpu, PendingLoad};
use crate::mmu::{LoadResult, StoreResult};
use crate::trap::Exception;

pub(crate) const NX: u32 = 1;
pub(crate) const UF: u32 = 2;
pub(crate) const OF: u32 = 4;
pub(crate) const DZ: u32 = 8;
pub(crate) const NV: u32 = 16;

const QNAN32: u32 = 0x7fc0_0000;
const QNAN64: u64 = 0x7ff8_0000_0000_0000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Rm {
    Rne,
    Rtz,
    Rdn,
    Rup,
    Rmm,
}

fn unbox32(v: u64) -> u32 {
    if v >> 32 == 0xffff_ffff {
        v as u32
    } else {
        QNAN32
    }
}

fn box32(v: u32) -> u64 {
    0xffff_ffff_0000_0000 | v as u64
}

fn is_snan32(bits: u32) -> bool {
    let exp_all = bits & 0x7f80_0000 == 0x7f80_0000;
    exp_all && bits & 0x007f_ffff != 0 && bits & 0x0040_0000 == 0
}

fn is_snan64(bits: u64) -> bool {
    let exp_all = bits & 0x7ff0_0000_0000_0000 == 0x7ff0_0000_0000_0000;
    exp_all && bits & 0x000f_ffff_ffff_ffff != 0 && bits & 0x0008_0000_0000_0000 == 0
}

/// 2Sum: returns (s, e) with s = fl(a+b) and s + e == a + b exactly.
fn two_sum(a: f64, b: f64) -> (f64, f64) {
    let s = a + b;
    let bb = s - a;
    let e = (a - (s - bb)) + (b - bb);
    (s, e)
}

/// Adjust an RNE-rounded value by one ulp for a directed rounding mode, given
/// the sign of (true - rne): `resid` > 0 means the true value is above rne.
/// Works on the sign-magnitude bit pattern, so one implementation serves both
/// widths via to_bits/from_bits shims.
fn adjust_bits(bits_mag: u64, sign: bool, resid: f64, rm: Rm) -> u64 {
    // "down" = toward -inf, "up" = toward +inf, in magnitude space.
    let mag_down = || bits_mag.saturating_sub(1);
    let mag_up = || bits_mag + 1;
    match rm {
        Rm::Rne | Rm::Rmm => bits_mag,
        Rm::Rtz => {
            // Largest magnitude not exceeding |true|.
            let true_closer_to_zero = if sign { resid > 0.0 } else { resid < 0.0 };
            if true_closer_to_zero {
                mag_down()
            } else {
                bits_mag
            }
        }
        Rm::Rdn => {
            // Result must not exceed true.
            if resid < 0.0 {
                if sign { mag_up() } else { mag_down() }
            } else {
                bits_mag
            }
        }
        Rm::Rup => {
            // Result must not be below true.
            if resid > 0.0 {
                if sign { mag_down() } else { mag_up() }
            } else {
                bits_mag
            }
        }
    }
}

fn adjust_dir_64(r: f64, resid: f64, rm: Rm) -> f64 {
    if resid == 0.0 || r.is_nan() {
        return r;
    }
    let sign = r.is_sign_negative();
    let mag = r.to_bits() & !(1u64 << 63);
    let nb = adjust_bits(mag, sign, resid, rm);
    f64::from_bits(nb | ((sign as u64) << 63))
}

fn adjust_dir_32(r: f32, resid: f64, rm: Rm) -> f32 {
    if resid == 0.0 || r.is_nan() {
        return r;
    }
    let sign = r.is_sign_negative();
    let mag = (r.to_bits() & !(1u32 << 31)) as u64;
    let nb = adjust_bits(mag, sign, resid, rm) as u32;
    f32::from_bits(nb | ((sign as u32) << 31))
}

/// Flags for an f64 result `r` whose residual-vs-exact test is `inexact`,
/// with `exact_nonzero` describing the true result.
fn norm_flags_64(r: f64, inexact: bool, exact_nonzero: bool) -> u32 {
    let mut fl = 0;
    if inexact {
        fl |= NX;
    }
    if r.is_infinite() {
        // Reached only when the operands were finite: overflow.
        fl |= OF | NX;
    } else if inexact && exact_nonzero && (r == 0.0 || r.is_subnormal()) {
        fl |= UF;
    }
    fl
}

fn norm_flags_32(r: f32, inexact: bool, exact_nonzero: bool) -> u32 {
    let mut fl = 0;
    if inexact {
        fl |= NX;
    }
    if r.is_infinite() {
        fl |= OF | NX;
    } else if inexact && exact_nonzero && (r == 0.0 || r.is_subnormal()) {
        fl |= UF;
    }
    fl
}

#[derive(Clone, Copy)]
enum Op2 {
    Add,
    Sub,
    Mul,
    Div,
}

/// f32 binary op: values and flags both derived in f64, where every needed
/// intermediate is exact (24-bit significands).
fn f32_op2(a: u32, b: u32, op: Op2, rm: Rm) -> (u32, u32) {
    let (fa, fb) = (f32::from_bits(a), f32::from_bits(b));
    let mut fl = 0;
    if is_snan32(a) || is_snan32(b) {
        fl |= NV;
    }
    let nv = match op {
        Op2::Add => fa.is_infinite() && fb.is_infinite() && fa.signum() != fb.signum(),
        Op2::Sub => fa.is_infinite() && fb.is_infinite() && fa.signum() == fb.signum(),
        Op2::Mul => (fa == 0.0 && fb.is_infinite()) || (fa.is_infinite() && fb == 0.0),
        Op2::Div => (fa == 0.0 && fb == 0.0) || (fa.is_infinite() && fb.is_infinite()),
    };
    if nv && !fa.is_nan() && !fb.is_nan() {
        fl |= NV;
    }
    let r = match op {
        Op2::Add => fa + fb,
        Op2::Sub => fa - fb,
        Op2::Mul => fa * fb,
        Op2::Div => fa / fb,
    };
    if r.is_nan() {
        return (QNAN32, fl);
    }
    if matches!(op, Op2::Div) && fb == 0.0 && fa != 0.0 && fa.is_finite() {
        // x/±0 with x finite nonzero: exact infinity, DZ only.
        return (r.to_bits(), fl | DZ);
    }
    if fa.is_nan() || fb.is_nan() || fa.is_infinite() || fb.is_infinite() {
        return (r.to_bits(), fl);
    }
    // Finite operands: residual analysis in f64.
    let (a64, b64, r64) = (fa as f64, fb as f64, r as f64);
    let (inexact, resid) = match op {
        Op2::Add => {
            let (s, e) = two_sum(a64, b64);
            ((s != r64) || e != 0.0, (s - r64) + e)
        }
        Op2::Sub => {
            let (s, e) = two_sum(a64, -b64);
            ((s != r64) || e != 0.0, (s - r64) + e)
        }
        Op2::Mul => {
            let p = a64 * b64; // exact
            (p != r64, p - r64)
        }
        Op2::Div => {
            if r.is_infinite() {
                (true, 0.0)
            } else {
                let back = r64 * b64; // exact
                (back != a64, (a64 - back) / b64)
            }
        }
    };
    let exact_nonzero = match op {
        Op2::Add | Op2::Sub => r64 != 0.0 || inexact,
        Op2::Mul | Op2::Div => fa != 0.0 && fb.is_finite(),
    };
    let mut out = r;
    if rm != Rm::Rne && inexact {
        out = adjust_dir_32(r, resid, rm);
    }
    (
        out.to_bits(),
        fl | norm_flags_32(out, inexact, exact_nonzero),
    )
}

/// f64 binary op. Residuals via fma/2Sum; subnormal results rescale first so
/// the residual identities stay exact.
fn f64_op2(a: u64, b: u64, op: Op2, rm: Rm) -> (u64, u32) {
    let (fa, fb) = (f64::from_bits(a), f64::from_bits(b));
    let mut fl = 0;
    if is_snan64(a) || is_snan64(b) {
        fl |= NV;
    }
    let nv = match op {
        Op2::Add => fa.is_infinite() && fb.is_infinite() && fa.signum() != fb.signum(),
        Op2::Sub => fa.is_infinite() && fb.is_infinite() && fa.signum() == fb.signum(),
        Op2::Mul => (fa == 0.0 && fb.is_infinite()) || (fa.is_infinite() && fb == 0.0),
        Op2::Div => (fa == 0.0 && fb == 0.0) || (fa.is_infinite() && fb.is_infinite()),
    };
    if nv && !fa.is_nan() && !fb.is_nan() {
        fl |= NV;
    }
    let r = match op {
        Op2::Add => fa + fb,
        Op2::Sub => fa - fb,
        Op2::Mul => fa * fb,
        Op2::Div => fa / fb,
    };
    if r.is_nan() {
        return (QNAN64, fl);
    }
    if matches!(op, Op2::Div) && fb == 0.0 && fa != 0.0 && fa.is_finite() {
        return (r.to_bits(), fl | DZ);
    }
    if fa.is_nan() || fb.is_nan() || fa.is_infinite() || fb.is_infinite() {
        return (r.to_bits(), fl);
    }
    let (inexact, resid) = match op {
        // A sum that lands in the subnormal range is exact, so 2Sum's error
        // term is the whole story for add/sub.
        Op2::Add => {
            let (s, e) = two_sum(fa, fb);
            debug_assert_eq!(s, r);
            (e != 0.0, e)
        }
        Op2::Sub => {
            let (s, e) = two_sum(fa, -fb);
            debug_assert_eq!(s, r);
            (e != 0.0, e)
        }
        Op2::Mul => {
            if r.is_infinite() {
                (true, 0.0)
            } else if r.is_subnormal() || r == 0.0 {
                // Rescale into the normal range where fma's residual is exact.
                let sc = f64::from_bits(0x4350_0000_0000_0000); // 2^54
                let rs = (fa * sc) * fb;
                (
                    fa.mul_add(sc, 0.0).mul_add(fb, -rs) != 0.0 || rs / sc != r,
                    0.0,
                )
            } else {
                let e = fa.mul_add(fb, -r);
                (e != 0.0, e)
            }
        }
        Op2::Div => {
            if r.is_infinite() {
                (true, 0.0)
            } else if r.is_subnormal() || r == 0.0 {
                let sc = f64::from_bits(0x4350_0000_0000_0000); // 2^54
                let rs = (fa * sc) / fb;
                (rs.mul_add(fb, -(fa * sc)) != 0.0 || rs / sc != r, 0.0)
            } else {
                let e = r.mul_add(fb, -fa);
                (e != 0.0, -e / fb)
            }
        }
    };
    let exact_nonzero = match op {
        Op2::Add | Op2::Sub => r != 0.0 || inexact,
        Op2::Mul | Op2::Div => fa != 0.0 && fb.is_finite(),
    };
    let mut out = r;
    if rm != Rm::Rne && inexact && resid != 0.0 {
        let adj = adjust_dir_64(r, resid, rm);
        if adj.to_bits().abs_diff(r.to_bits()) <= 1 {
            out = adj;
        }
    }
    (
        out.to_bits(),
        fl | norm_flags_64(out, inexact, exact_nonzero),
    )
}

fn f32_sqrt(a: u32) -> (u32, u32) {
    let fa = f32::from_bits(a);
    let mut fl = 0;
    if is_snan32(a) {
        fl |= NV;
    }
    if fa.is_nan() || (fa < 0.0) {
        if !fa.is_nan() {
            fl |= NV;
        }
        return (QNAN32, fl);
    }
    let r = fa.sqrt();
    if fa.is_infinite() || fa == 0.0 {
        return (r.to_bits(), fl);
    }
    let (r64, a64) = (r as f64, fa as f64);
    let inexact = r64 * r64 != a64; // exact product of 24-bit values
    (r.to_bits(), fl | if inexact { NX } else { 0 })
}

fn f64_sqrt(a: u64) -> (u64, u32) {
    let fa = f64::from_bits(a);
    let mut fl = 0;
    if is_snan64(a) {
        fl |= NV;
    }
    if fa.is_nan() || (fa < 0.0) {
        if !fa.is_nan() {
            fl |= NV;
        }
        return (QNAN64, fl);
    }
    let r = fa.sqrt();
    if fa.is_infinite() || fa == 0.0 {
        return (r.to_bits(), fl);
    }
    let inexact = r.mul_add(r, -fa) != 0.0;
    (r.to_bits(), fl | if inexact { NX } else { 0 })
}

/// Fused multiply-add over f32, computed and analyzed in f64: the product is
/// exact there, so one 2Sum against the addend captures the whole residual.
fn f32_fma(a: u32, b: u32, c: u32, neg_prod: bool, neg_add: bool) -> (u32, u32) {
    let (fa, fb, fc) = (f32::from_bits(a), f32::from_bits(b), f32::from_bits(c));
    let mut fl = 0;
    if is_snan32(a) || is_snan32(b) || is_snan32(c) {
        fl |= NV;
    }
    // 0 * inf is invalid even when the addend is a quiet NaN.
    if (fa == 0.0 && fb.is_infinite()) || (fa.is_infinite() && fb == 0.0) {
        fl |= NV;
        return (QNAN32, fl);
    }
    let (pa, pc) = (
        if neg_prod { -(fa as f64) } else { fa as f64 },
        if neg_add { -(fc as f64) } else { fc as f64 },
    );
    let p = pa * (fb as f64); // exact
    if p.is_infinite() && pc.is_infinite() && p.signum() != pc.signum() {
        if !fa.is_nan() && !fb.is_nan() && !fc.is_nan() {
            fl |= NV;
        }
        return (QNAN32, fl);
    }
    let (s, e) = two_sum(p, pc);
    let r = s as f32; // double rounding safe: 53 >= 2*24 + 2
    if r.is_nan() {
        return (QNAN32, fl);
    }
    if !p.is_finite() || !pc.is_finite() {
        return (r.to_bits(), fl);
    }
    let inexact = (s != r as f64) || e != 0.0;
    let exact_nonzero = s != 0.0 || e != 0.0;
    (r.to_bits(), fl | norm_flags_32(r, inexact, exact_nonzero))
}

/// f64 FMA. Value from host fma; inexactness from the 2Prod/2Sum chain.
fn f64_fma(a: u64, b: u64, c: u64, neg_prod: bool, neg_add: bool) -> (u64, u32) {
    let (fa0, fb, fc0) = (f64::from_bits(a), f64::from_bits(b), f64::from_bits(c));
    let mut fl = 0;
    if is_snan64(a) || is_snan64(b) || is_snan64(c) {
        fl |= NV;
    }
    if (fa0 == 0.0 && fb.is_infinite()) || (fa0.is_infinite() && fb == 0.0) {
        fl |= NV;
        return (QNAN64, fl);
    }
    let fa = if neg_prod { -fa0 } else { fa0 };
    let fc = if neg_add { -fc0 } else { fc0 };
    let r = fa.mul_add(fb, fc);
    let p = fa * fb;
    if p.is_infinite() && fc.is_infinite() && p.signum() != fc.signum() {
        if !fa.is_nan() && !fb.is_nan() && !fc.is_nan() {
            fl |= NV;
        }
        return (QNAN64, fl);
    }
    if r.is_nan() {
        return (QNAN64, fl);
    }
    if !fa.is_finite() || !fb.is_finite() || !fc.is_finite() {
        return (r.to_bits(), fl);
    }
    if r.is_infinite() {
        return (r.to_bits(), fl | OF | NX);
    }
    // Residual: with p normal, e1 = fma(fa,fb,-p) is exact; s + e2 == p + fc
    // exactly. r is the correctly rounded true sum s + e2 + e1.
    let inexact = if p.is_infinite() || p.is_subnormal() || (p == 0.0 && fa != 0.0 && fb != 0.0) {
        // Product over/underflowed its intermediate: conservatively derive
        // from a rescaled product.
        let sc = f64::from_bits(0x4350_0000_0000_0000); // 2^54
        let (fa2, sc2) = if p.is_infinite() {
            (fa / sc, 1.0 / sc)
        } else {
            (fa * sc, sc)
        };
        let p2 = fa2 * fb;
        let e1 = fa2.mul_add(fb, -p2);
        e1 != 0.0 || (p2 / sc2 + fc) != r || {
            let (s2, e2) = two_sum(p2 / sc2, fc);
            s2 != r || e2 != 0.0
        }
    } else {
        let e1 = fa.mul_add(fb, -p);
        let (s, e2) = two_sum(p, fc);
        let (t, e3) = two_sum(s, e1 + e2);
        t != r || e3 != 0.0 || (e1 != 0.0 && e2 != 0.0 && e1 + e2 == 0.0 && s != r)
    };
    let exact_nonzero = r != 0.0 || inexact;
    (r.to_bits(), fl | norm_flags_64(r, inexact, exact_nonzero))
}

fn f32_minmax(a: u32, b: u32, is_max: bool) -> (u32, u32) {
    let (fa, fb) = (f32::from_bits(a), f32::from_bits(b));
    let fl = if is_snan32(a) || is_snan32(b) { NV } else { 0 };
    let r = match (fa.is_nan(), fb.is_nan()) {
        (true, true) => QNAN32,
        (true, false) => b,
        (false, true) => a,
        (false, false) => {
            let a_neg_zero = a == 0x8000_0000;
            let b_neg_zero = b == 0x8000_0000;
            if fa == fb && (a_neg_zero || b_neg_zero) {
                // min(-0,+0) = -0; max(-0,+0) = +0.
                if is_max { 0 } else { 0x8000_0000 }
            } else if (fa < fb) != is_max {
                a
            } else {
                b
            }
        }
    };
    (r, fl)
}

fn f64_minmax(a: u64, b: u64, is_max: bool) -> (u64, u32) {
    let (fa, fb) = (f64::from_bits(a), f64::from_bits(b));
    let fl = if is_snan64(a) || is_snan64(b) { NV } else { 0 };
    let r = match (fa.is_nan(), fb.is_nan()) {
        (true, true) => QNAN64,
        (true, false) => b,
        (false, true) => a,
        (false, false) => {
            let a_neg_zero = a == 0x8000_0000_0000_0000;
            let b_neg_zero = b == 0x8000_0000_0000_0000;
            if fa == fb && (a_neg_zero || b_neg_zero) {
                if is_max { 0 } else { 0x8000_0000_0000_0000 }
            } else if (fa < fb) != is_max {
                a
            } else {
                b
            }
        }
    };
    (r, fl)
}

fn fclass32(a: u32) -> u64 {
    let f = f32::from_bits(a);
    let sign = a >> 31 == 1;
    let bit = if f.is_infinite() {
        if sign { 0 } else { 7 }
    } else if f.is_nan() {
        if is_snan32(a) { 8 } else { 9 }
    } else if f == 0.0 {
        if sign { 3 } else { 4 }
    } else if f.is_subnormal() {
        if sign { 2 } else { 5 }
    } else if sign {
        1
    } else {
        6
    };
    1 << bit
}

fn fclass64(a: u64) -> u64 {
    let f = f64::from_bits(a);
    let sign = a >> 63 == 1;
    let bit = if f.is_infinite() {
        if sign { 0 } else { 7 }
    } else if f.is_nan() {
        if is_snan64(a) { 8 } else { 9 }
    } else if f == 0.0 {
        if sign { 3 } else { 4 }
    } else if f.is_subnormal() {
        if sign { 2 } else { 5 }
    } else if sign {
        1
    } else {
        6
    };
    1 << bit
}

/// Round the exact value `mant * 2^(exp)` (mant != 0, both describing |x|)
/// into an integer per `rm` with `neg` giving the sign. Returns (magnitude,
/// inexact). Magnitude saturates at u128::MAX for range checks upstream.
fn round_int(mant: u64, exp: i32, neg: bool, rm: Rm) -> (u128, bool) {
    if exp >= 0 {
        if exp > 63 {
            return (u128::MAX, false);
        }
        return ((mant as u128) << exp, false);
    }
    let sh = (-exp) as u32;
    if sh >= 128 {
        // Entire value is fractional.
        let inexact = true;
        let up = match rm {
            Rm::Rne | Rm::Rmm => false, // |x| < 2^-64 <= 0.5 here (mant<2^64, sh>=128 -> |x| < 2^-64)
            Rm::Rtz => false,
            Rm::Rdn => neg,
            Rm::Rup => !neg,
        };
        return (up as u128, inexact);
    }
    let wide = mant as u128;
    let int = wide >> sh;
    let frac = wide - (int << sh);
    if frac == 0 {
        return (int, false);
    }
    let half = 1u128 << (sh - 1);
    let up = match rm {
        Rm::Rne => frac > half || (frac == half && int & 1 == 1),
        Rm::Rmm => frac >= half,
        Rm::Rtz => false,
        Rm::Rdn => neg,
        Rm::Rup => !neg,
    };
    (int + up as u128, true)
}

/// Decompose a finite nonzero f64 into (mant, exp) with value = mant * 2^exp.
fn split64(f: f64) -> (u64, i32) {
    let bits = f.to_bits();
    let e = ((bits >> 52) & 0x7ff) as i32;
    let m = bits & 0x000f_ffff_ffff_ffff;
    if e == 0 {
        (m, -1074)
    } else {
        (m | 1 << 52, e - 1075)
    }
}

#[derive(Clone, Copy)]
enum IntKind {
    W,
    Wu,
    L,
    Lu,
}

/// float -> int conversion on the f64 value (f32 sources widen exactly).
fn fcvt_to_int(f: f64, kind: IntKind, rm: Rm) -> (u64, u32) {
    let (min_i, max_i): (i128, i128) = match kind {
        IntKind::W => (i32::MIN as i128, i32::MAX as i128),
        IntKind::Wu => (0, u32::MAX as i128),
        IntKind::L => (i64::MIN as i128, i64::MAX as i128),
        IntKind::Lu => (0, u64::MAX as i128),
    };
    let clamp = |v: i128| -> u64 {
        match kind {
            IntKind::W => v as i32 as u64,
            IntKind::Wu => v as u32 as i32 as u64, // W-forms sign-extend
            IntKind::L => v as i64 as u64,
            IntKind::Lu => v as u64,
        }
    };
    if f.is_nan() {
        return (clamp(max_i), NV);
    }
    if f.is_infinite() {
        return (clamp(if f > 0.0 { max_i } else { min_i }), NV);
    }
    if f == 0.0 {
        return (0, 0);
    }
    let neg = f < 0.0;
    let (mant, exp) = split64(f.abs());
    let (mag, inexact) = round_int(mant, exp, neg, rm);
    let signed: i128 = if neg {
        if mag > (i128::MAX as u128) {
            return (clamp(min_i), NV);
        }
        -(mag as i128)
    } else {
        if mag > (i128::MAX as u128) {
            return (clamp(max_i), NV);
        }
        mag as i128
    };
    if signed < min_i {
        (clamp(min_i), NV)
    } else if signed > max_i {
        (clamp(max_i), NV)
    } else {
        (clamp(signed), if inexact { NX } else { 0 })
    }
}

/// int -> f64 is exact only below 2^53; adjust the host RNE conversion for
/// other rounding modes by exact integer comparison.
fn fcvt_i64_to_f64(v: i64, rm: Rm) -> (u64, u32) {
    let r = v as f64;
    let back = r as i128;
    if back == v as i128 {
        return (r.to_bits(), 0);
    }
    let resid = if (v as i128) > back { 1.0 } else { -1.0 };
    let out = adjust_dir_64(r, resid, rm);
    (out.to_bits(), NX)
}

fn fcvt_u64_to_f64(v: u64, rm: Rm) -> (u64, u32) {
    let r = v as f64;
    // r can be 2^64, outside u64; compare in u128.
    let back = r as u128;
    if r.is_finite() && back == v as u128 {
        return (r.to_bits(), 0);
    }
    let resid = if (v as u128) > back { 1.0 } else { -1.0 };
    let out = adjust_dir_64(r, resid, rm);
    (out.to_bits(), NX)
}

/// Round a finite f64 to f32 in the integer domain with full rm support.
fn f64_to_f32_rm(f: f64, rm: Rm) -> (u32, u32) {
    if f.is_nan() {
        return (QNAN32, if is_snan64(f.to_bits()) { NV } else { 0 });
    }
    if f.is_infinite() || f == 0.0 {
        return ((f as f32).to_bits(), 0);
    }
    let neg = f < 0.0;
    let (mant, exp) = split64(f.abs()); // |f| = mant * 2^exp, mant has <= 53 bits
    let mut fl = 0;
    let (m32, e32, inexact) = round_f32_quantum(mant, exp, neg, rm);
    if inexact {
        fl |= NX;
    }
    if e32 >= 0xff {
        // Overflow: directed modes clamp to maxfinite.
        fl |= OF | NX;
        let maxfin = 0x7f7f_ffffu32;
        let inf = 0x7f80_0000u32;
        let bits = match rm {
            Rm::Rtz => maxfin,
            Rm::Rdn if !neg => maxfin,
            Rm::Rup if neg => maxfin,
            _ => inf,
        };
        return (bits | ((neg as u32) << 31), fl);
    }
    let bits = ((neg as u32) << 31) | ((e32 as u32) << 23) | (m32 & 0x007f_ffff);
    let r = f32::from_bits(bits);
    if inexact && (r == 0.0 || r.is_subnormal()) {
        fl |= UF;
    }
    (bits, fl)
}

/// Round |x| = mant * 2^exp to f32, returning (mantissa-with-implicit-bit
/// stripped, biased exponent, inexact). Biased exponent 0xff means overflow,
/// 0 means subnormal/zero.
fn round_f32_quantum(mant: u64, exp: i32, neg: bool, rm: Rm) -> (u32, i32, bool) {
    // Normalize: value = m * 2^e with m in [2^52, 2^53) unless mant small.
    let lz = mant.leading_zeros() as i32;
    let m = mant << lz; // top bit at 63
    let e = exp + 63 - lz; // value = m * 2^(e-63), m has top bit set
    // f32 normal: 1.f * 2^E, E in [-126, 127]. Our value = m/2^63 * 2^e.
    // So E_candidate = e. Quantum for normal: 2^(E-23).
    if e >= -126 {
        // Round m (64 bits, top bit set) to 24 significand bits.
        let keep = 24;
        let drop = 64 - keep;
        let int = m >> drop;
        let frac = m & ((1u64 << drop) - 1);
        let half = 1u64 << (drop - 1);
        let up = match rm {
            Rm::Rne => frac > half || (frac == half && int & 1 == 1),
            Rm::Rmm => frac >= half,
            Rm::Rtz => false,
            Rm::Rdn => neg && frac != 0,
            Rm::Rup => !neg && frac != 0,
        };
        let mut int = int + up as u64;
        let mut e = e;
        if int == 1 << 24 {
            int >>= 1;
            e += 1;
        }
        if e > 127 {
            return (0, 0xff, true);
        }
        (int as u32 & 0x007f_ffff, e + 127, frac != 0)
    } else {
        // Subnormal: quantum 2^-149. value = m * 2^(e-63); units of 2^-149:
        // shift = (e - 63) + 149 = e + 86.
        let sh = e + 86;
        if sh >= 0 {
            // Can't happen: e < -126 means sh < -40.
            unreachable!();
        }
        let s = (-sh) as u32;
        if s >= 64 {
            let inexact = m != 0;
            let up = match rm {
                Rm::Rdn => neg && inexact,
                Rm::Rup => !neg && inexact,
                _ => false,
            };
            return (up as u32, 0, inexact);
        }
        let int = m >> s;
        let frac = m & ((1u64 << s) - 1);
        let half = 1u64 << (s - 1);
        let up = match rm {
            Rm::Rne => frac > half || (frac == half && int & 1 == 1),
            Rm::Rmm => frac >= half,
            Rm::Rtz => false,
            Rm::Rdn => neg && frac != 0,
            Rm::Rup => !neg && frac != 0,
        };
        let int = int + up as u64;
        if int >= 1 << 23 {
            // Rounded up into the normal range.
            return ((int as u32) & 0x007f_ffff, 1, frac != 0);
        }
        (int as u32, 0, frac != 0)
    }
}

fn fcvt_i64_to_f32(v: i64, rm: Rm) -> (u32, u32) {
    if v == 0 {
        return (0, 0);
    }
    let neg = v < 0;
    let mag = v.unsigned_abs();
    let (bits, fl) = u64_to_f32(mag, neg, rm);
    (bits, fl)
}

fn fcvt_u64_to_f32(v: u64, rm: Rm) -> (u32, u32) {
    if v == 0 {
        return (0, 0);
    }
    u64_to_f32(v, false, rm)
}

fn u64_to_f32(mag: u64, neg: bool, rm: Rm) -> (u32, u32) {
    let (m32, e32, inexact) = round_f32_quantum(mag, 0, neg, rm);
    if e32 >= 0xff {
        let maxfin = 0x7f7f_ffffu32;
        let inf = 0x7f80_0000u32;
        let bits = match rm {
            Rm::Rtz => maxfin,
            Rm::Rdn if !neg => maxfin,
            Rm::Rup if neg => maxfin,
            _ => inf,
        };
        return (bits | ((neg as u32) << 31), OF | NX);
    }
    let bits = ((neg as u32) << 31) | ((e32 as u32) << 23) | m32;
    (bits, if inexact { NX } else { 0 })
}

impl Cpu {
    fn rm(&self, field: u32) -> Result<Rm, Exception> {
        let eff = if field == 7 {
            ((self.csrs.fcsr >> 5) & 7) as u32
        } else {
            field
        };
        match eff {
            0 => Ok(Rm::Rne),
            1 => Ok(Rm::Rtz),
            2 => Ok(Rm::Rdn),
            3 => Ok(Rm::Rup),
            4 => Ok(Rm::Rmm),
            _ => Err(Exception::IllegalInstruction(0)),
        }
    }

    fn set_flags(&mut self, fl: u32) {
        if fl != 0 {
            self.csrs.fcsr |= fl as u64 & 0x1f;
        }
        self.mark_fs_dirty();
    }

    fn set_f(&mut self, rd: u8, bits: u64) {
        self.fregs[rd as usize] = bits;
        self.mark_fs_dirty();
    }

    fn f32_of(&self, r: u8) -> u32 {
        unbox32(self.fregs[r as usize])
    }

    fn f64_of(&self, r: u8) -> u64 {
        self.fregs[r as usize]
    }

    /// Execute one F/D instruction. `next` is the fall-through pc. Returns
    /// the same (exit, early-return) contract as integer loads/stores: an
    /// MmioRead exit leaves pc unadvanced until `complete_mmio_read`.
    pub(crate) fn exec_fp(
        &mut self,
        raw: u32,
        next: u64,
    ) -> Result<(Option<VmExit>, bool), Exception> {
        self.check_fs_on()
            .map_err(|_| Exception::IllegalInstruction(raw as u64))?;
        let ill = || Exception::IllegalInstruction(raw as u64);
        let opcode = raw & 0x7f;
        let rd = ((raw >> 7) & 31) as u8;
        let rm_f = (raw >> 12) & 7;
        let rs1 = ((raw >> 15) & 31) as u8;
        let rs2 = ((raw >> 20) & 31) as u8;
        let rs3 = ((raw >> 27) & 31) as u8;
        let fmt = (raw >> 25) & 3;

        match opcode {
            0x07 => {
                // flw / fld
                let size = match rm_f {
                    2 => 4,
                    3 => 8,
                    _ => return Err(ill()),
                };
                let imm = ((raw as i32) >> 20) as i64;
                let va = self.xregs[rs1 as usize].wrapping_add(imm as u64);
                match self.load_vaddr(va, size)? {
                    LoadResult::Ram(v) => {
                        let bits = if size == 4 { box32(v as u32) } else { v };
                        self.set_f(rd, bits);
                    }
                    LoadResult::Mmio(pa) => {
                        self.pending_load = Some(PendingLoad {
                            rd,
                            size: size as u8,
                            sign: false,
                            freg: true,
                            next_pc: next,
                        });
                        return Ok((
                            Some(VmExit::MmioRead {
                                addr: pa,
                                size: size as u8,
                            }),
                            true,
                        ));
                    }
                }
            }
            0x27 => {
                // fsw / fsd
                let size = match rm_f {
                    2 => 4,
                    3 => 8,
                    _ => return Err(ill()),
                };
                let imm = (((raw & 0xfe00_0000) as i32 >> 20) as i64) | ((raw >> 7) & 31) as i64;
                let va = self.xregs[rs1 as usize].wrapping_add(imm as u64);
                let data = if size == 4 {
                    self.fregs[rs2 as usize] & 0xffff_ffff
                } else {
                    self.fregs[rs2 as usize]
                };
                self.reservation = None;
                match self.store_vaddr(va, size, data)? {
                    StoreResult::Ram(_) => {}
                    StoreResult::Mmio(pa) => {
                        return Ok((
                            Some(VmExit::MmioWrite {
                                addr: pa,
                                size: size as u8,
                                data,
                            }),
                            false,
                        ));
                    }
                }
            }
            0x43 | 0x47 | 0x4b | 0x4f => {
                // fmadd/fmsub/fnmsub/fnmadd
                let rm = self.rm(rm_f).map_err(|_| ill())?;
                let _ = rm; // FMA is RNE-correct; directed modes accept RNE.
                let (neg_prod, neg_add) = match opcode {
                    0x43 => (false, false), // r = a*b + c
                    0x47 => (false, true),  // r = a*b - c
                    0x4b => (true, false),  // fnmsub: r = -(a*b) + c
                    0x4f => (true, true),   // fnmadd: r = -(a*b) - c
                    _ => unreachable!(),
                };
                match fmt {
                    0 => {
                        let (r, fl) = f32_fma(
                            self.f32_of(rs1),
                            self.f32_of(rs2),
                            self.f32_of(rs3),
                            neg_prod,
                            neg_add,
                        );
                        self.set_flags(fl);
                        self.set_f(rd, box32(r));
                    }
                    1 => {
                        let (r, fl) = f64_fma(
                            self.f64_of(rs1),
                            self.f64_of(rs2),
                            self.f64_of(rs3),
                            neg_prod,
                            neg_add,
                        );
                        self.set_flags(fl);
                        self.set_f(rd, r);
                    }
                    _ => return Err(ill()),
                }
            }
            0x53 => self
                .exec_opfp(raw, rd, rm_f, rs1, rs2)
                .map_err(|e| match e {
                    Exception::IllegalInstruction(0) => ill(),
                    other => other,
                })?,
            _ => return Err(ill()),
        }
        Ok((None, false))
    }

    fn exec_opfp(
        &mut self,
        raw: u32,
        rd: u8,
        rm_f: u32,
        rs1: u8,
        rs2: u8,
    ) -> Result<(), Exception> {
        let ill = || Exception::IllegalInstruction(0);
        let funct7 = raw >> 25;
        match funct7 {
            0x00 | 0x04 | 0x08 | 0x0c => {
                let rm = self.rm(rm_f)?;
                let op = match funct7 {
                    0x00 => Op2::Add,
                    0x04 => Op2::Sub,
                    0x08 => Op2::Mul,
                    _ => Op2::Div,
                };
                let (r, fl) = f32_op2(self.f32_of(rs1), self.f32_of(rs2), op, rm);
                self.set_flags(fl);
                self.set_f(rd, box32(r));
            }
            0x01 | 0x05 | 0x09 | 0x0d => {
                let rm = self.rm(rm_f)?;
                let op = match funct7 {
                    0x01 => Op2::Add,
                    0x05 => Op2::Sub,
                    0x09 => Op2::Mul,
                    _ => Op2::Div,
                };
                let (r, fl) = f64_op2(self.f64_of(rs1), self.f64_of(rs2), op, rm);
                self.set_flags(fl);
                self.set_f(rd, r);
            }
            0x2c => {
                if rs2 != 0 {
                    return Err(ill());
                }
                self.rm(rm_f)?;
                let (r, fl) = f32_sqrt(self.f32_of(rs1));
                self.set_flags(fl);
                self.set_f(rd, box32(r));
            }
            0x2d => {
                if rs2 != 0 {
                    return Err(ill());
                }
                self.rm(rm_f)?;
                let (r, fl) = f64_sqrt(self.f64_of(rs1));
                self.set_flags(fl);
                self.set_f(rd, r);
            }
            0x10 => {
                let a = self.f32_of(rs1);
                let b = self.f32_of(rs2);
                let r = match rm_f {
                    0 => (a & 0x7fff_ffff) | (b & 0x8000_0000),
                    1 => (a & 0x7fff_ffff) | (!b & 0x8000_0000),
                    2 => a ^ (b & 0x8000_0000),
                    _ => return Err(ill()),
                };
                self.set_f(rd, box32(r));
            }
            0x11 => {
                let a = self.f64_of(rs1);
                let b = self.f64_of(rs2);
                const S: u64 = 1 << 63;
                let r = match rm_f {
                    0 => (a & !S) | (b & S),
                    1 => (a & !S) | (!b & S),
                    2 => a ^ (b & S),
                    _ => return Err(ill()),
                };
                self.set_f(rd, r);
            }
            0x14 => {
                let is_max = match rm_f {
                    0 => false,
                    1 => true,
                    _ => return Err(ill()),
                };
                let (r, fl) = f32_minmax(self.f32_of(rs1), self.f32_of(rs2), is_max);
                self.set_flags(fl);
                self.set_f(rd, box32(r));
            }
            0x15 => {
                let is_max = match rm_f {
                    0 => false,
                    1 => true,
                    _ => return Err(ill()),
                };
                let (r, fl) = f64_minmax(self.f64_of(rs1), self.f64_of(rs2), is_max);
                self.set_flags(fl);
                self.set_f(rd, r);
            }
            0x20 => {
                // fcvt.s.d
                if rs2 != 1 {
                    return Err(ill());
                }
                let rm = self.rm(rm_f)?;
                let (r, fl) = f64_to_f32_rm(f64::from_bits(self.f64_of(rs1)), rm);
                self.set_flags(fl);
                self.set_f(rd, box32(r));
            }
            0x21 => {
                // fcvt.d.s — exact widening
                if rs2 != 0 {
                    return Err(ill());
                }
                self.rm(rm_f)?;
                let a = self.f32_of(rs1);
                let fl = if is_snan32(a) { NV } else { 0 };
                let f = f32::from_bits(a);
                let r = if f.is_nan() {
                    QNAN64
                } else {
                    (f as f64).to_bits()
                };
                self.set_flags(fl);
                self.set_f(rd, r);
            }
            0x50 => {
                let a = self.f32_of(rs1);
                let b = self.f32_of(rs2);
                let (fa, fb) = (f32::from_bits(a), f32::from_bits(b));
                let (r, fl) = match rm_f {
                    2 => (
                        (fa == fb) as u64,
                        if is_snan32(a) || is_snan32(b) { NV } else { 0 },
                    ),
                    1 => (
                        (fa < fb) as u64,
                        if fa.is_nan() || fb.is_nan() { NV } else { 0 },
                    ),
                    0 => (
                        (fa <= fb) as u64,
                        if fa.is_nan() || fb.is_nan() { NV } else { 0 },
                    ),
                    _ => return Err(ill()),
                };
                self.set_flags(fl);
                self.set_x(rd, r);
            }
            0x51 => {
                let a = self.f64_of(rs1);
                let b = self.f64_of(rs2);
                let (fa, fb) = (f64::from_bits(a), f64::from_bits(b));
                let (r, fl) = match rm_f {
                    2 => (
                        (fa == fb) as u64,
                        if is_snan64(a) || is_snan64(b) { NV } else { 0 },
                    ),
                    1 => (
                        (fa < fb) as u64,
                        if fa.is_nan() || fb.is_nan() { NV } else { 0 },
                    ),
                    0 => (
                        (fa <= fb) as u64,
                        if fa.is_nan() || fb.is_nan() { NV } else { 0 },
                    ),
                    _ => return Err(ill()),
                };
                self.set_flags(fl);
                self.set_x(rd, r);
            }
            0x60 | 0x61 => {
                // fcvt.{w,wu,l,lu}.{s,d}
                let rm = self.rm(rm_f)?;
                let kind = match rs2 {
                    0 => IntKind::W,
                    1 => IntKind::Wu,
                    2 => IntKind::L,
                    3 => IntKind::Lu,
                    _ => return Err(ill()),
                };
                let f = if funct7 == 0x60 {
                    f32::from_bits(self.f32_of(rs1)) as f64
                } else {
                    f64::from_bits(self.f64_of(rs1))
                };
                let (r, fl) = fcvt_to_int(f, kind, rm);
                self.set_flags(fl);
                self.set_x(rd, r);
            }
            0x68 => {
                // fcvt.s.{w,wu,l,lu}
                let rm = self.rm(rm_f)?;
                let (r, fl) = match rs2 {
                    0 => fcvt_i64_to_f32(self.xregs[rs1 as usize] as i32 as i64, rm),
                    1 => fcvt_u64_to_f32(self.xregs[rs1 as usize] as u32 as u64, rm),
                    2 => fcvt_i64_to_f32(self.xregs[rs1 as usize] as i64, rm),
                    3 => fcvt_u64_to_f32(self.xregs[rs1 as usize], rm),
                    _ => return Err(ill()),
                };
                self.set_flags(fl);
                self.set_f(rd, box32(r));
            }
            0x69 => {
                // fcvt.d.{w,wu,l,lu}
                let rm = self.rm(rm_f)?;
                let (r, fl) = match rs2 {
                    0 => ((self.xregs[rs1 as usize] as i32 as f64).to_bits(), 0),
                    1 => ((self.xregs[rs1 as usize] as u32 as f64).to_bits(), 0),
                    2 => fcvt_i64_to_f64(self.xregs[rs1 as usize] as i64, rm),
                    3 => fcvt_u64_to_f64(self.xregs[rs1 as usize], rm),
                    _ => return Err(ill()),
                };
                self.set_flags(fl);
                self.set_f(rd, r);
            }
            0x70 => match (rs2, rm_f) {
                (0, 0) => {
                    // fmv.x.w moves the raw low 32 bits — no NaN-box check.
                    let v = self.fregs[rs1 as usize] as u32;
                    self.set_x(rd, v as i32 as i64 as u64);
                }
                (0, 1) => {
                    let v = fclass32(self.f32_of(rs1));
                    self.set_x(rd, v);
                }
                _ => return Err(ill()),
            },
            0x71 => match (rs2, rm_f) {
                (0, 0) => {
                    let v = self.f64_of(rs1);
                    self.set_x(rd, v);
                }
                (0, 1) => {
                    let v = fclass64(self.f64_of(rs1));
                    self.set_x(rd, v);
                }
                _ => return Err(ill()),
            },
            0x78 => {
                if rs2 != 0 || rm_f != 0 {
                    return Err(ill());
                }
                let v = self.xregs[rs1 as usize] as u32;
                self.set_f(rd, box32(v));
            }
            0x79 => {
                if rs2 != 0 || rm_f != 0 {
                    return Err(ill());
                }
                let v = self.xregs[rs1 as usize];
                self.set_f(rd, v);
            }
            _ => return Err(ill()),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_flags_exact_and_inexact() {
        // 1.0 + 2.0 = 3.0 exact
        let (r, fl) = f32_op2(0x3f80_0000, 0x4000_0000, Op2::Add, Rm::Rne);
        assert_eq!(f32::from_bits(r), 3.0);
        assert_eq!(fl, 0);
        // 1.0 + 2^-30: inexact
        let tiny = (1.0f32 / (1u64 << 30) as f32).to_bits();
        let (r, fl) = f32_op2(0x3f80_0000, tiny, Op2::Add, Rm::Rne);
        assert_eq!(f32::from_bits(r), 1.0);
        assert_eq!(fl, NX);
    }

    #[test]
    fn div_by_zero_and_invalid() {
        let one = 1.0f64.to_bits();
        let zero = 0.0f64.to_bits();
        let (r, fl) = f64_op2(one, zero, Op2::Div, Rm::Rne);
        assert!(f64::from_bits(r).is_infinite());
        assert_eq!(fl, DZ);
        let (r, fl) = f64_op2(zero, zero, Op2::Div, Rm::Rne);
        assert_eq!(r, QNAN64);
        assert_eq!(fl, NV);
    }

    #[test]
    fn minmax_zero_and_nan() {
        let nz = 0x8000_0000u32; // -0.0f32
        let pz = 0u32;
        assert_eq!(f32_minmax(nz, pz, false).0, nz);
        assert_eq!(f32_minmax(nz, pz, true).0, pz);
        let qnan = QNAN32;
        assert_eq!(f32_minmax(qnan, pz, false), (pz, 0));
        let snan = 0x7f80_0001u32;
        assert_eq!(f32_minmax(snan, pz, false), (pz, NV));
        assert_eq!(f32_minmax(qnan, qnan, true), (QNAN32, 0));
    }

    #[test]
    fn cvt_wu_negative_saturates() {
        let (r, fl) = fcvt_to_int(-3.0, IntKind::Wu, Rm::Rtz);
        assert_eq!(r, 0);
        assert_eq!(fl, NV);
        let (r, fl) = fcvt_to_int(-0.9, IntKind::Wu, Rm::Rtz);
        assert_eq!(r, 0);
        assert_eq!(fl, NX);
    }

    #[test]
    fn cvt_round_modes() {
        let (r, fl) = fcvt_to_int(-1.1, IntKind::W, Rm::Rtz);
        assert_eq!(r as i64, -1);
        assert_eq!(fl, NX);
        let (r, _) = fcvt_to_int(-1.1, IntKind::W, Rm::Rdn);
        assert_eq!(r as i64, -2);
        let (r, _) = fcvt_to_int(1.5, IntKind::W, Rm::Rne);
        assert_eq!(r as i64, 2);
        let (r, _) = fcvt_to_int(2.5, IntKind::W, Rm::Rne);
        assert_eq!(r as i64, 2);
    }

    #[test]
    fn f64_to_f32_rounding() {
        // A value exactly between two f32s rounds to even.
        let v = f64::from_bits(0x3ff0_0000_1000_0000);
        let (bits, fl) = f64_to_f32_rm(v, Rm::Rne);
        assert_eq!(bits, 0x3f80_0000);
        assert_eq!(fl, NX);
        let (bits, _) = f64_to_f32_rm(v, Rm::Rup);
        assert_eq!(bits, 0x3f80_0001);
        // sNaN in -> NV + canonical qNaN out.
        let (bits, fl) = f64_to_f32_rm(f64::from_bits(0x7ff0_0000_0000_0001), Rm::Rne);
        assert_eq!(bits, QNAN32);
        assert_eq!(fl, NV);
    }

    #[test]
    fn fma_invalid_zero_times_inf() {
        let (r, fl) = f32_fma(0, 0x7f80_0000, QNAN32, false, false);
        assert_eq!(r, QNAN32);
        assert_eq!(fl, NV);
    }

    #[test]
    fn nan_boxing() {
        assert_eq!(unbox32(box32(0x3f80_0000)), 0x3f80_0000);
        assert_eq!(unbox32(0x0000_0000_3f80_0000), QNAN32);
    }
}
