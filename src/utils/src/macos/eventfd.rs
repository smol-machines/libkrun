// Copyright 2021 Sergio Lopez. All rights reserved.
//
// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
//
// SPDX-License-Identifier: (Apache-2.0 AND BSD-3-Clause)

//! Structure and wrapper functions emulating eventfd using a pipe.
//!
//! The pipe is an anonymous FIFO opened for reading and writing, so each
//! eventfd costs one file descriptor instead of a pipe's two. A VMM holds
//! more than a hundred eventfds, and macOS caps a process at
//! `kern.maxfilesperproc` descriptors, which also bound the guest's
//! connections (one host socket each). Where the FIFO cannot be made (no
//! writable temporary directory), it falls back to a pipe.

use std::ffi::CString;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::{io, result};

use nix::errno::Errno;
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::unistd::{dup, pipe, read, write};

pub const EFD_NONBLOCK: i32 = 1;

// NOTE: We introduce the semaphore flag here, but we don't implement the semantics exactly - We don't
//       return the correct value from read().
pub const EFD_SEMAPHORE: i32 = 2;

fn set_nonblock(fd: &OwnedFd) -> result::Result<(), io::Error> {
    let flags = fcntl(fd, FcntlArg::F_GETFL)?;
    let flags = OFlag::from_bits(flags).ok_or(io::ErrorKind::InvalidData)?;
    fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;

    Ok(())
}

fn clear_nonblock(fd: &OwnedFd) -> result::Result<(), io::Error> {
    let flags = fcntl(fd, FcntlArg::F_GETFL)?;
    let flags = OFlag::from_bits(flags).ok_or(io::ErrorKind::InvalidData)?;
    fcntl(fd, FcntlArg::F_SETFL(flags - OFlag::O_NONBLOCK))?;

    Ok(())
}

/// A FIFO opened for both reading and writing, with no name left behind.
fn anonymous_fifo() -> result::Result<OwnedFd, io::Error> {
    let mut template = std::env::temp_dir()
        .join("krun-eventfd-XXXXXX")
        .as_os_str()
        .as_bytes()
        .to_vec();
    template.push(0);
    // SAFETY: `template` is a writable NUL-terminated buffer mkdtemp fills in.
    let dir = unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) };
    if dir.is_null() {
        return Err(io::Error::last_os_error());
    }
    template.pop();
    let mut path = template.clone();
    path.extend_from_slice(b"/e");
    let dir = CString::new(template).map_err(|_| io::ErrorKind::InvalidInput)?;
    let path = CString::new(path).map_err(|_| io::ErrorKind::InvalidInput)?;
    // SAFETY: both are valid NUL-terminated paths; the FIFO and its directory
    // are removed before returning, whatever happens.
    let fd = unsafe {
        let fd = if libc::mkfifo(path.as_ptr(), 0o600) == 0 {
            // Opening a FIFO for reading and writing never waits for a peer.
            libc::open(
                path.as_ptr(),
                libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        } else {
            -1
        };
        let error = io::Error::last_os_error();
        libc::unlink(path.as_ptr());
        libc::rmdir(dir.as_ptr());
        if fd < 0 {
            return Err(error);
        }
        OwnedFd::from_raw_fd(fd)
    };
    Ok(fd)
}

#[derive(Debug)]
pub struct EventFd {
    read_fd: OwnedFd,
    /// The write end when it is a separate descriptor (the pipe fallback);
    /// `None` when `read_fd` is a FIFO that is written to as well.
    write_fd: Option<OwnedFd>,
}

impl EventFd {
    pub fn new(flag: i32) -> result::Result<EventFd, io::Error> {
        if let Ok(fd) = anonymous_fifo() {
            if flag != EFD_NONBLOCK {
                clear_nonblock(&fd)?;
            }
            return Ok(EventFd {
                read_fd: fd,
                write_fd: None,
            });
        }

        let (read_fd, write_fd) = pipe()?;

        if flag == EFD_NONBLOCK {
            set_nonblock(&read_fd)?;
            set_nonblock(&write_fd)?;
        }

        Ok(EventFd {
            read_fd,
            write_fd: Some(write_fd),
        })
    }

    fn writer(&self) -> std::os::fd::BorrowedFd<'_> {
        self.write_fd.as_ref().unwrap_or(&self.read_fd).as_fd()
    }

    pub fn write(&self, v: u64) -> result::Result<(), io::Error> {
        match write(self.writer(), &v.to_le_bytes()) {
            // We may get EAGAIN if the eventfd is overstimulated, but we can safely
            // ignore it as we can be sure the subscriber will get notified.
            Ok(_) | Err(Errno::EAGAIN) => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    pub fn read(&self) -> result::Result<u64, io::Error> {
        let mut buf: [u8; 8] = [0; 8];
        read(&self.read_fd, &mut buf)?;
        Ok(u64::from_le_bytes(buf))
    }

    pub fn try_clone(&self) -> result::Result<EventFd, io::Error> {
        let read_fd = dup(&self.read_fd)?;
        let write_fd = self.write_fd.as_ref().map(dup).transpose()?;
        Ok(EventFd { read_fd, write_fd })
    }

    pub fn get_write_fd(&self) -> RawFd {
        self.writer().as_raw_fd()
    }
}

impl AsRawFd for EventFd {
    fn as_raw_fd(&self) -> RawFd {
        self.read_fd.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        EventFd::new(EFD_NONBLOCK).unwrap();
        EventFd::new(0).unwrap();
    }

    #[test]
    fn test_read_write() {
        let evt = EventFd::new(EFD_NONBLOCK).unwrap();
        evt.write(55).unwrap();
        assert_eq!(evt.read().unwrap(), 55);
    }

    #[test]
    fn test_read_nothing() {
        let evt = EventFd::new(EFD_NONBLOCK).unwrap();
        let r = evt.read();
        match r {
            Err(ref inner) if inner.kind() == io::ErrorKind::WouldBlock => (),
            _ => panic!("Unexpected"),
        }
    }
    #[test]
    fn one_descriptor_carries_both_ends() {
        let evt = EventFd::new(EFD_NONBLOCK).unwrap();
        assert!(evt.write_fd.is_none());
        assert_eq!(evt.get_write_fd(), evt.as_raw_fd());
        // A write through the descriptor handed out as the write end, as a
        // C caller of krun_get_shutdown_eventfd does, wakes the reader.
        let v: u64 = 7;
        let written = unsafe { libc::write(evt.get_write_fd(), (&v as *const u64).cast(), 8) };
        assert_eq!(written, 8);
        assert_eq!(evt.read().unwrap(), 7);
        assert!(evt.read().is_err());
    }

    #[test]
    fn a_blocking_eventfd_waits_for_a_write() {
        let evt = EventFd::new(0).unwrap();
        let writer = evt.try_clone().unwrap();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            writer.write(3).unwrap();
        });
        assert_eq!(evt.read().unwrap(), 3);
        handle.join().unwrap();
    }

    #[test]
    fn test_clone() {
        let evt = EventFd::new(EFD_NONBLOCK).unwrap();
        let evt_clone = evt.try_clone().unwrap();
        evt.write(923).unwrap();
        assert_eq!(evt_clone.read().unwrap(), 923);
    }
}
