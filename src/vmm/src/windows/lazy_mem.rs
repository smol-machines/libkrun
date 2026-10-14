// Copyright 2026 The libkrun Authors. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Guest memory that is reserved up front and committed on first touch.
//!
//! Windows has no overcommit: committed memory is charged against the system
//! commit limit whether or not it is ever touched, so committing a region the
//! guest may never use (a virtiofs DAX window, whose file data lives in views
//! WHP maps over it) costs its full size per VM. A region registered here is
//! only reserved. WHP accepts reserved memory in `WHvMapGpaRange`; a guest
//! access to an uncommitted page exits with `MemoryAccess` (GPA mapped), and
//! [`commit_gpa`] commits the page so the instruction completes on resume. A
//! host access (anything reading or writing through the region's host address)
//! faults in user mode and is committed by a vectored exception handler.

use std::ffi::c_void;
use std::sync::{Once, RwLock};

use windows_sys::Win32::Foundation::EXCEPTION_ACCESS_VIOLATION;
use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_POINTERS,
};
use windows_sys::Win32::System::Memory::{VirtualAlloc, MEM_COMMIT, PAGE_READWRITE};

/// Commit granularity: one allocation-granularity block per fault keeps the
/// exit count low (16 pages per exit) without committing much the guest did
/// not ask for.
const CHUNK: u64 = 64 * 1024;

const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;
const EXCEPTION_CONTINUE_SEARCH: i32 = 0;

#[derive(Clone, Copy)]
struct Range {
    gpa: u64,
    host: u64,
    size: u64,
}

static RANGES: RwLock<Vec<Range>> = RwLock::new(Vec::new());
static HANDLER: Once = Once::new();

/// Register a reserved region so guest and host touches commit it on demand.
pub fn register(gpa: u64, host: *mut u8, size: usize) {
    HANDLER.call_once(|| {
        // SAFETY: the handler only reads the exception record and commits pages
        // inside registered, still-reserved ranges.
        unsafe { AddVectoredExceptionHandler(1, Some(on_access_violation)) };
    });
    RANGES.write().unwrap().push(Range {
        gpa,
        host: host as u64,
        size: size as u64,
    });
}

/// Commit the chunk holding `start + offset` of `range`, clamped to the range.
fn commit(range: &Range, offset: u64) -> bool {
    let chunk_start = offset & !(CHUNK - 1);
    let len = CHUNK.min(range.size - chunk_start);
    // SAFETY: the address lies inside a reservation this process owns, and
    // committing an already-committed page is a no-op that keeps its contents.
    let addr = unsafe {
        VirtualAlloc(
            (range.host + chunk_start) as *const c_void,
            len as usize,
            MEM_COMMIT,
            PAGE_READWRITE,
        )
    };
    !addr.is_null()
}

/// Called on a `MemoryAccess` exit for a mapped GPA: if the GPA lies in a
/// registered region, commit its chunk and return true so the vCPU resumes and
/// retries the instruction. False means the access is not ours (MMIO).
pub fn commit_gpa(gpa: u64) -> bool {
    let ranges = RANGES.read().unwrap();
    match ranges
        .iter()
        .find(|r| gpa >= r.gpa && gpa - r.gpa < r.size)
    {
        Some(range) => commit(range, gpa - range.gpa),
        None => false,
    }
}

unsafe extern "system" fn on_access_violation(info: *mut EXCEPTION_POINTERS) -> i32 {
    // SAFETY: the system passes a valid EXCEPTION_POINTERS to a vectored handler.
    let record = unsafe { &*(*info).ExceptionRecord };
    if record.ExceptionCode != EXCEPTION_ACCESS_VIOLATION || record.NumberParameters < 2 {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let addr = record.ExceptionInformation[1] as u64;
    // A writer holds the lock only while registering at startup; never block a
    // faulting thread on it.
    let Ok(ranges) = RANGES.try_read() else {
        return EXCEPTION_CONTINUE_SEARCH;
    };
    match ranges
        .iter()
        .find(|r| addr >= r.host && addr - r.host < r.size)
    {
        Some(range) if commit(range, addr - range.host) => EXCEPTION_CONTINUE_EXECUTION,
        _ => EXCEPTION_CONTINUE_SEARCH,
    }
}
