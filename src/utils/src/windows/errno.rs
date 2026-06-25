// Copyright 2026 The libkrun Authors. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Minimal Windows replacement for `vmm_sys_util::errno` (which is Unix-only).
//!
//! Provides an `Error` type with the same surface the rest of the tree relies on
//! (a numeric OS error wrapper that is `Display`/`Error`, convertible from
//! [`std::io::Error`]).

use std::fmt::{self, Display, Formatter};
use std::io;

/// An OS-level error, mirroring `vmm_sys_util::errno::Error`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error(i32);

impl Error {
    /// Construct from a raw OS error code.
    pub fn new(errno: i32) -> Self {
        Error(errno)
    }

    /// The last OS error (`GetLastError` on Windows, via `io::Error`).
    pub fn last() -> Self {
        Error::from(io::Error::last_os_error())
    }

    /// The wrapped raw OS error code.
    pub fn errno(self) -> i32 {
        self.0
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "{}", io::Error::from_raw_os_error(self.0))
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error(e.raw_os_error().unwrap_or(0))
    }
}

impl From<Error> for io::Error {
    fn from(e: Error) -> Self {
        io::Error::from_raw_os_error(e.0)
    }
}

/// Convenience result alias matching `vmm_sys_util::errno::Result`.
pub type Result<T> = std::result::Result<T, Error>;
