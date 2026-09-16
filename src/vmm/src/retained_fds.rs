// Copyright 2026. SPDX-License-Identifier: Apache-2.0
//! Bounded, same-UID capability transfer for immutable checkpoint backings.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const TOKEN_LEN: usize = 32;
const TIMEOUT: Duration = Duration::from_millis(500);
const HANDOFF_DEADLINE: Duration = Duration::from_secs(2);

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .map(|left| left.min(TIMEOUT))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "checkpoint descriptor handoff deadline exceeded",
            )
        })
}

pub struct RetainedFiles {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    path: PathBuf,
    identity: (u64, u64),
}

fn identity(path: &Path) -> io::Result<(u64, u64)> {
    let metadata = fs::symlink_metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

fn peer(stream: &UnixStream) -> io::Result<libc::ucred> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of_val(&credentials) as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut size,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    if size as usize != std::mem::size_of_val(&credentials) {
        return Err(io::Error::other("invalid checkpoint peer credentials"));
    }
    Ok(credentials)
}

fn immutable_descriptor(file: &File) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if flags & libc::O_ACCMODE == libc::O_RDONLY {
        return Ok(());
    }
    let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
    let required = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK;
    if seals < 0 || seals & required != required {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "checkpoint handoff requires read-only files or sealed memory",
        ));
    }
    Ok(())
}

impl RetainedFiles {
    pub fn start(
        path: &Path,
        files: BTreeMap<i32, Arc<File>>,
    ) -> io::Result<(Self, [u8; TOKEN_LEN])> {
        if files.is_empty() || files.len() > 1024 || path.as_os_str().len() > 100 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid checkpoint handoff size or path",
            ));
        }
        for file in files.values() {
            immutable_descriptor(file)?;
        }
        let mut token = [0; TOKEN_LEN];
        let mut read = 0;
        while read < token.len() {
            let result = unsafe {
                libc::getrandom(token[read..].as_mut_ptr().cast(), token.len() - read, 0)
            };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if result == 0 {
                return Err(io::Error::other(
                    "checkpoint token generation returned no bytes",
                ));
            }
            read += result as usize;
        }
        let listener = UnixListener::bind(path)?;
        let id = identity(path)?;
        let stop = Arc::new(AtomicBool::new(false));
        let mut service = Self {
            stop: stop.clone(),
            worker: None,
            path: path.into(),
            identity: id,
        };
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let path = path.to_path_buf();
        let uid = unsafe { libc::geteuid() };
        service.worker = Some(thread::Builder::new().name("checkpoint-fds".into()).spawn(
            move || {
                while !stop.load(Ordering::Acquire) && identity(&path).ok() == Some(id) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let result = (|| -> io::Result<()> {
                                if peer(&stream)?.uid != uid {
                                    return Err(io::Error::new(
                                        io::ErrorKind::PermissionDenied,
                                        "checkpoint peer UID mismatch",
                                    ));
                                }
                                let deadline = Instant::now() + HANDOFF_DEADLINE;
                                let mut supplied = [0; TOKEN_LEN];
                                let mut offset = 0;
                                while offset < supplied.len() {
                                    stream.set_read_timeout(Some(remaining(deadline)?))?;
                                    match stream.read(&mut supplied[offset..]) {
                                        Ok(0) => {
                                            return Err(io::Error::new(
                                                io::ErrorKind::UnexpectedEof,
                                                "checkpoint client disconnected",
                                            ));
                                        }
                                        Ok(count) => offset += count,
                                        Err(error)
                                            if error.kind() == io::ErrorKind::Interrupted =>
                                        {
                                            continue;
                                        }
                                        Err(error) => return Err(error),
                                    }
                                }
                                if !supplied
                                    .iter()
                                    .zip(token)
                                    .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                                    .eq(&0)
                                {
                                    return Err(io::Error::new(
                                        io::ErrorKind::PermissionDenied,
                                        "checkpoint token mismatch",
                                    ));
                                }
                                for file in files.values() {
                                    if stop.load(Ordering::Acquire) {
                                        break;
                                    }
                                    send_fd(&stream, file.as_raw_fd(), deadline)?;
                                }
                                Ok(())
                            })();
                            if let Err(error) = result {
                                log::debug!(
                                    "checkpoint descriptor handoff did not complete: {error}"
                                );
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5))
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
            },
        )?);
        Ok((service, token))
    }

    pub fn is_finished(&self) -> bool {
        self.worker
            .as_ref()
            .is_none_or(|worker| worker.is_finished())
    }
}

impl Drop for RetainedFiles {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if identity(&self.path).ok() == Some(self.identity) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub fn receive(
    path: &Path,
    token: &[u8; TOKEN_LEN],
    owner: u32,
    keys: &[i32],
) -> io::Result<BTreeMap<i32, Arc<File>>> {
    if keys.is_empty() || keys.len() > 1024 {
        return Err(io::Error::other("invalid checkpoint descriptor count"));
    }
    let mut stream = UnixStream::connect(path)?;
    let credentials = peer(&stream)?;
    if credentials.pid as u32 != owner || credentials.uid != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "checkpoint owner credentials mismatch",
        ));
    }
    let deadline = Instant::now() + HANDOFF_DEADLINE;
    stream.set_write_timeout(Some(TIMEOUT))?;
    stream.write_all(token)?;
    let mut files = BTreeMap::new();
    for key in keys {
        let file = File::from(receive_fd(&stream, deadline)?);
        immutable_descriptor(&file)?;
        files.insert(*key, Arc::new(file));
    }
    Ok(files)
}

fn send_fd(stream: &UnixStream, fd: RawFd, deadline: Instant) -> io::Result<()> {
    let mut byte = b'F';
    let mut iov = libc::iovec {
        iov_base: (&mut byte as *mut u8).cast(),
        iov_len: 1,
    };
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0usize; space.div_ceil(std::mem::size_of::<usize>())];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space;
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&msg);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as usize;
        std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<RawFd>(), fd);
    }
    loop {
        stream.set_write_timeout(Some(remaining(deadline)?))?;
        let result = unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) };
        if result == 1 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if result < 0 && error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(if result < 0 {
            error
        } else {
            io::Error::new(io::ErrorKind::WriteZero, "checkpoint fd send incomplete")
        });
    }
}

fn receive_fd(stream: &UnixStream, deadline: Instant) -> io::Result<OwnedFd> {
    let mut byte = 0u8;
    let mut iov = libc::iovec {
        iov_base: (&mut byte as *mut u8).cast(),
        iov_len: 1,
    };
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0usize; space.div_ceil(std::mem::size_of::<usize>())];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space;
    let result = loop {
        stream.set_read_timeout(Some(remaining(deadline)?))?;
        let result = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
        if result < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        break result;
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut received = Vec::new();
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&msg);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let length = (*header)
                    .cmsg_len
                    .saturating_sub(libc::CMSG_LEN(0) as usize);
                for offset in (0..length).step_by(std::mem::size_of::<RawFd>()) {
                    if offset + std::mem::size_of::<RawFd>() > length {
                        break;
                    }
                    let fd = std::ptr::read_unaligned(
                        libc::CMSG_DATA(header).add(offset).cast::<RawFd>(),
                    );
                    received.push(OwnedFd::from_raw_fd(fd));
                }
            }
            header = libc::CMSG_NXTHDR(&msg, header);
        }
    }
    if result != 1 || byte != b'F' || msg.msg_flags & libc::MSG_CTRUNC != 0 || received.len() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid checkpoint fd handoff",
        ));
    }
    Ok(received.pop().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::FileExt;
    use std::time::Instant;

    fn path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("krun-fds-{label}-{}", std::process::id()))
    }

    fn retained_file() -> File {
        let file = crate::builder::create_guest_ram_memfd(4096).unwrap();
        file.write_all_at(b"retained", 0).unwrap();
        let readonly = File::open(format!("/proc/self/fd/{}", file.as_raw_fd())).unwrap();
        file.set_permissions(fs::Permissions::from_mode(0o000))
            .unwrap();
        readonly
    }

    #[test]
    fn handoff_preserves_contents_without_reopening_inode() {
        let path = path("private");
        let file = retained_file();
        let (service, token) =
            RetainedFiles::start(&path, BTreeMap::from([(42, Arc::new(file))])).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        let files = receive(&path, &token, std::process::id(), &[42]).unwrap();
        drop(service);
        assert!(!path.exists());
        let mut bytes = [0; 8];
        files[&42].read_exact_at(&mut bytes, 0).unwrap();
        assert_eq!(&bytes, b"retained");
        assert!(files[&42].write_all_at(b"changed", 0).is_err());
        assert_ne!(
            unsafe { libc::fcntl(files[&42].as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
    }

    #[test]
    fn rejected_client_does_not_prevent_next_handoff() {
        let path = path("rejection");
        let (service, token) =
            RetainedFiles::start(&path, BTreeMap::from([(8, Arc::new(retained_file()))])).unwrap();
        let mut wrong = token;
        wrong[0] ^= 1;
        assert!(receive(&path, &wrong, std::process::id(), &[8]).is_err());
        assert!(receive(&path, &token, std::process::id() + 1, &[8]).is_err());
        assert!(receive(&path, &token, std::process::id(), &[8]).is_ok());
        let stalled = UnixStream::connect(&path).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let start = Instant::now();
        drop(service);
        assert!(start.elapsed() < Duration::from_secs(2));
        drop(stalled);
    }

    #[test]
    fn writable_unsealed_files_are_not_exported() {
        let path = path("unsealed");
        let file = crate::builder::create_guest_ram_memfd(4096).unwrap();
        assert!(RetainedFiles::start(&path, BTreeMap::from([(0, Arc::new(file))])).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn replacing_socket_retires_service_without_deleting_replacement() {
        let path = path("replacement");
        let (service, _) =
            RetainedFiles::start(&path, BTreeMap::from([(1, Arc::new(retained_file()))])).unwrap();
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"replacement").unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !service.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(service.is_finished());
        drop(service);
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn expired_handoff_deadline_fails_before_waiting_for_bytes() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let deadline = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            receive_fd(&stream, deadline).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(
            send_fd(&stream, stream.as_raw_fd(), deadline)
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
    }
}
