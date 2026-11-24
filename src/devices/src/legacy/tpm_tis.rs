// SPDX-License-Identifier: Apache-2.0

//! A TPM 2.0 with the TIS (FIFO) register interface, backed by swtpm.
//!
//! The guest's `tpm_tis` driver finds it at [`TPM_TIS_ADDR`] (on x86_64 with
//! `tpm_tis.force=1`, as there is no ACPI table) and polls it
//! (`tpm_tis.interrupts=0`). Each command the guest writes to the FIFO is sent
//! to swtpm (`swtpm socket --tpm2 --ctrl type=unixio,path=...`) when the guest
//! sets tpmGo, and the response is read back through the FIFO.
//!
//! Only locality 0 is served; the others read as an invalid locality. The
//! register layout and state machine follow the TCG PC Client Platform TPM
//! Profile, as QEMU's `tpm-tis` does.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use crate::bus::BusDevice;

/// Guest-physical base of the TIS register space.
pub const TPM_TIS_ADDR: u64 = 0xFED4_0000;
/// Five localities of 4 KiB each.
pub const TPM_TIS_SIZE: u64 = 0x5000;

const LOCALITY_SHIFT: u64 = 12;

const REG_ACCESS: u64 = 0x00;
const REG_INT_ENABLE: u64 = 0x08;
const REG_INT_VECTOR: u64 = 0x0c;
const REG_INT_STATUS: u64 = 0x10;
const REG_INTF_CAPABILITY: u64 = 0x14;
const REG_STS: u64 = 0x18;
const REG_DATA_FIFO: u64 = 0x24;
const REG_INTERFACE_ID: u64 = 0x30;
const REG_DATA_XFIFO: u64 = 0x80;
const REG_DATA_XFIFO_END: u64 = 0xbc;
const REG_DID_VID: u64 = 0xf00;
const REG_RID: u64 = 0xf04;

const STS_TPM_FAMILY2_0: u32 = 1 << 26;
const STS_VALID: u32 = 1 << 7;
const STS_COMMAND_READY: u32 = 1 << 6;
const STS_TPM_GO: u32 = 1 << 5;
const STS_DATA_AVAILABLE: u32 = 1 << 4;
const STS_EXPECT: u32 = 1 << 3;
const STS_SELFTEST_DONE: u32 = 1 << 2;
const STS_RESPONSE_RETRY: u32 = 1 << 1;

const ACCESS_TPM_REG_VALID_STS: u8 = 1 << 7;
const ACCESS_ACTIVE_LOCALITY: u8 = 1 << 5;
const ACCESS_REQUEST_USE: u8 = 1 << 1;

/// Interface version 1.3 for TPM 2.0, 64-byte transfers, dynamic burst count,
/// low-level interrupts (mandatory to advertise; the guest polls).
const INTF_CAPABILITY: u32 = (3 << 28) | (3 << 9) | (1 << 4);
/// FIFO interface for TPM 2.0, five localities, TIS supported.
const INTERFACE_ID: u32 = (1 << 8) | (1 << 13);
/// The IDs QEMU's tpm-tis reports.
const DID_VID: u32 = (0x0001 << 16) | 0x1014;
const RID: u32 = 0x01;

/// Largest command or response the TPM takes, per the TCG reference.
const BUFFER_MAX: usize = 4096;
/// What a read of the FIFO returns when there is nothing to read.
const NO_DATA_BYTE: u8 = 0xff;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Ready,
    Reception,
    Completion,
}

/// Where commands go: swtpm's data channel, or anything that answers a TPM
/// command with a TPM response (tests).
pub trait TpmBackend: Send {
    fn execute(&mut self, command: &[u8]) -> io::Result<Vec<u8>>;
}

/// swtpm, reached through its control socket (`--ctrl type=unixio,path=...`)
/// the way QEMU's `tpm-emulator` backend reaches it: a socket pair is handed
/// to swtpm as its data channel (`CMD_SET_DATAFD`), then the TPM is
/// initialized (`CMD_INIT`).
pub struct SwtpmSocket {
    stream: UnixStream,
    /// Kept open: swtpm serves the data channel while it is.
    _ctrl: UnixStream,
}

const CMD_INIT: u32 = 0x02;
const CMD_SET_DATAFD: u32 = 0x10;

impl SwtpmSocket {
    pub fn connect(ctrl_path: &Path) -> io::Result<Self> {
        use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};
        use std::os::fd::AsRawFd;

        let mut ctrl = UnixStream::connect(ctrl_path)?;
        let (stream, theirs) = UnixStream::pair()?;

        let command = CMD_SET_DATAFD.to_be_bytes();
        let fds = [theirs.as_raw_fd()];
        sendmsg::<()>(
            ctrl.as_raw_fd(),
            &[io::IoSlice::new(&command)],
            &[ControlMessage::ScmRights(&fds)],
            MsgFlags::empty(),
            None,
        )
        .map_err(io::Error::from)?;
        Self::check(&mut ctrl, "CMD_SET_DATAFD")?;
        drop(theirs);

        let mut init = CMD_INIT.to_be_bytes().to_vec();
        init.extend_from_slice(&0u32.to_be_bytes());
        ctrl.write_all(&init)?;
        Self::check(&mut ctrl, "CMD_INIT")?;

        Ok(Self {
            stream,
            _ctrl: ctrl,
        })
    }

    /// Read a control command's 4-byte result: 0 is success.
    fn check(ctrl: &mut UnixStream, command: &str) -> io::Result<()> {
        let mut result = [0u8; 4];
        ctrl.read_exact(&mut result)?;
        match u32::from_be_bytes(result) {
            0 => Ok(()),
            code => Err(io::Error::other(format!("swtpm {command}: {code:#x}"))),
        }
    }
}

impl TpmBackend for SwtpmSocket {
    fn execute(&mut self, command: &[u8]) -> io::Result<Vec<u8>> {
        self.stream.write_all(command)?;
        // Every response starts with a tag (2 bytes) and its own size (4).
        let mut header = [0u8; 6];
        self.stream.read_exact(&mut header)?;
        let size = u32::from_be_bytes(header[2..6].try_into().unwrap()) as usize;
        if !(header.len()..=BUFFER_MAX).contains(&size) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("swtpm response of {size} bytes"),
            ));
        }
        let mut response = header.to_vec();
        response.resize(size, 0);
        self.stream.read_exact(&mut response[header.len()..])?;
        Ok(response)
    }
}

/// A TPM 2.0 response with the given return code and no parameters.
fn error_response(code: u32) -> Vec<u8> {
    let mut response = vec![0x80, 0x01, 0, 0, 0, 10];
    response.extend_from_slice(&code.to_be_bytes());
    response
}

/// The size a TPM command or response states in its header.
fn stated_size(buffer: &[u8]) -> Option<usize> {
    buffer
        .get(2..6)
        .map(|size| u32::from_be_bytes(size.try_into().unwrap()) as usize)
}

pub struct TpmTis {
    backend: Box<dyn TpmBackend>,
    state: State,
    sts: u32,
    access: u8,
    active: bool,
    int_enable: u32,
    buffer: Vec<u8>,
    /// Bytes written so far (Reception) or read so far (Completion).
    offset: usize,
}

impl TpmTis {
    pub fn new(backend: Box<dyn TpmBackend>) -> Self {
        Self {
            backend,
            state: State::Idle,
            sts: STS_TPM_FAMILY2_0 | STS_SELFTEST_DONE,
            access: ACCESS_TPM_REG_VALID_STS,
            active: false,
            int_enable: 0,
            buffer: Vec::with_capacity(BUFFER_MAX),
            offset: 0,
        }
    }

    /// Keep the TPM family and self-test bits, replace the rest.
    fn set_sts(&mut self, flags: u32) {
        self.sts &= STS_SELFTEST_DONE | STS_TPM_FAMILY2_0;
        self.sts |= flags;
    }

    fn execute(&mut self) {
        let response = match self.backend.execute(&self.buffer) {
            Ok(response) => response,
            Err(error) => {
                log::error!("tpm: swtpm did not answer: {error}");
                // TPM_RC_FAILURE
                error_response(0x101)
            }
        };
        self.buffer = response;
        self.offset = 0;
        self.state = State::Completion;
        self.set_sts(STS_VALID | STS_DATA_AVAILABLE);
    }

    fn read_register(&mut self, register: u64, size: usize) -> u32 {
        match register {
            REG_ACCESS => {
                let mut access = self.access;
                if self.active {
                    access |= ACCESS_ACTIVE_LOCALITY;
                }
                u32::from(access)
            }
            REG_INT_ENABLE => self.int_enable,
            REG_INT_VECTOR | REG_INT_STATUS => 0,
            REG_INTF_CAPABILITY => INTF_CAPABILITY,
            REG_STS if self.active => {
                let burst = if self.sts & STS_DATA_AVAILABLE != 0 {
                    self.buffer.len().saturating_sub(self.offset)
                } else {
                    let available = BUFFER_MAX.saturating_sub(self.offset);
                    // A byte-sized read must not see 0x100 as 0x00.
                    if size == 1 {
                        available.min(0xff)
                    } else {
                        available
                    }
                };
                (burst.min(0xffff) as u32) << 8 | self.sts
            }
            REG_INTERFACE_ID => INTERFACE_ID,
            REG_DID_VID => DID_VID,
            REG_RID => RID,
            _ => 0xffff_ffff,
        }
    }

    fn read_fifo(&mut self) -> u8 {
        if self.state != State::Completion || self.sts & STS_DATA_AVAILABLE == 0 {
            return NO_DATA_BYTE;
        }
        let byte = self.buffer[self.offset];
        self.offset += 1;
        if self.offset >= self.buffer.len() {
            self.set_sts(STS_VALID);
        }
        byte
    }

    fn write_fifo(&mut self, byte: u8) {
        match self.state {
            State::Ready => {
                self.state = State::Reception;
                self.buffer.clear();
                self.offset = 0;
                self.set_sts(STS_EXPECT | STS_VALID);
            }
            State::Reception => {}
            // Dropped, as the spec has it.
            State::Idle | State::Completion => return,
        }
        if self.sts & STS_EXPECT == 0 || self.buffer.len() >= BUFFER_MAX {
            self.set_sts(STS_VALID);
            return;
        }
        self.buffer.push(byte);
        self.offset = self.buffer.len();
        if stated_size(&self.buffer).is_some_and(|size| self.buffer.len() >= size) {
            // Packet complete: no more bytes expected.
            self.set_sts(STS_VALID);
        }
    }

    fn write_sts(&mut self, value: u32) {
        match value & (STS_COMMAND_READY | STS_TPM_GO | STS_RESPONSE_RETRY) {
            STS_COMMAND_READY => match self.state {
                State::Idle | State::Completion | State::Reception => {
                    self.state = State::Ready;
                    self.buffer.clear();
                    self.offset = 0;
                    self.set_sts(STS_COMMAND_READY);
                }
                State::Ready => self.offset = 0,
            },
            STS_TPM_GO => {
                if self.state == State::Reception && self.sts & STS_EXPECT == 0 {
                    self.execute();
                }
            }
            STS_RESPONSE_RETRY if self.state == State::Completion => {
                self.offset = 0;
                self.set_sts(STS_VALID | STS_DATA_AVAILABLE);
            }
            _ => {}
        }
    }

    fn write_access(&mut self, value: u8) {
        if value & ACCESS_REQUEST_USE != 0 {
            self.active = true;
        }
        if value & ACCESS_ACTIVE_LOCALITY != 0 && self.active {
            // Relinquish: an unfinished command is dropped.
            self.active = false;
            self.state = State::Idle;
            self.buffer.clear();
            self.offset = 0;
            self.set_sts(0);
        }
    }
}

impl BusDevice for TpmTis {
    fn read(&mut self, _vcpuid: u64, offset: u64, data: &mut [u8]) {
        data.fill(0xff);
        if offset >> LOCALITY_SHIFT != 0 {
            return;
        }
        let register = offset & 0xfff;
        let fifo =
            register == REG_DATA_FIFO || (REG_DATA_XFIFO..=REG_DATA_XFIFO_END).contains(&register);
        if fifo {
            if !self.active {
                return;
            }
            for byte in data.iter_mut() {
                *byte = self.read_fifo();
            }
            return;
        }
        let aligned = register & !0x3;
        let shift = (register & 0x3) * 8;
        let value = self.read_register(aligned, data.len()) >> shift;
        for (i, byte) in data.iter_mut().enumerate().take(4) {
            *byte = (value >> (8 * i)) as u8;
        }
    }

    fn write(&mut self, _vcpuid: u64, offset: u64, data: &[u8]) {
        if offset >> LOCALITY_SHIFT != 0 || data.is_empty() {
            return;
        }
        let register = offset & 0xfff;
        let fifo =
            register == REG_DATA_FIFO || (REG_DATA_XFIFO..=REG_DATA_XFIFO_END).contains(&register);
        if fifo {
            if self.active {
                for &byte in data {
                    self.write_fifo(byte);
                }
            }
            return;
        }
        let shift = (register & 0x3) * 8;
        let mut value = 0u32;
        for (i, &byte) in data.iter().enumerate().take(4) {
            value |= u32::from(byte) << (8 * i);
        }
        let value = value << shift;
        match register & !0x3 {
            REG_ACCESS => self.write_access(value as u8),
            REG_INT_ENABLE => self.int_enable = value,
            REG_STS if self.active => self.write_sts(value),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers every command with a response that echoes its command code.
    struct Echo;

    impl TpmBackend for Echo {
        fn execute(&mut self, command: &[u8]) -> io::Result<Vec<u8>> {
            let mut response = vec![0x80, 0x01, 0, 0, 0, 14, 0, 0, 0, 0];
            response.extend_from_slice(&command[6..10]);
            Ok(response)
        }
    }

    fn read32(tpm: &mut TpmTis, offset: u64) -> u32 {
        let mut data = [0u8; 4];
        tpm.read(0, offset, &mut data);
        u32::from_le_bytes(data)
    }

    fn read8(tpm: &mut TpmTis, offset: u64) -> u8 {
        let mut data = [0u8; 1];
        tpm.read(0, offset, &mut data);
        data[0]
    }

    // TPM2_GetRandom(8): a 12-byte command, as the guest driver sends it.
    const GET_RANDOM: [u8; 12] = [0x80, 0x01, 0, 0, 0, 12, 0, 0, 0x01, 0x7b, 0, 8];

    #[test]
    fn a_command_goes_through_the_fifo_and_its_response_comes_back() {
        let mut tpm = TpmTis::new(Box::new(Echo));
        assert_eq!(read32(&mut tpm, REG_DID_VID), DID_VID);
        assert_eq!(read32(&mut tpm, REG_INTF_CAPABILITY) >> 28, 3, "TPM 2.0");

        // Request locality 0, then commandReady.
        tpm.write(0, REG_ACCESS, &[ACCESS_REQUEST_USE]);
        assert_ne!(read8(&mut tpm, REG_ACCESS) & ACCESS_ACTIVE_LOCALITY, 0);
        tpm.write(0, REG_STS, &STS_COMMAND_READY.to_le_bytes());
        assert_ne!(read32(&mut tpm, REG_STS) & STS_COMMAND_READY, 0);

        // Write all but the last byte: expect stays set.
        tpm.write(0, REG_DATA_FIFO, &GET_RANDOM[..11]);
        let sts = read32(&mut tpm, REG_STS);
        assert_ne!(sts & STS_EXPECT, 0, "more bytes expected");
        tpm.write(0, REG_DATA_FIFO, &GET_RANDOM[11..]);
        assert_eq!(read32(&mut tpm, REG_STS) & STS_EXPECT, 0, "packet complete");

        // tpmGo runs it; the response is then available, with its length as
        // the burst count.
        tpm.write(0, REG_STS, &STS_TPM_GO.to_le_bytes());
        let sts = read32(&mut tpm, REG_STS);
        assert_ne!(sts & STS_DATA_AVAILABLE, 0);
        assert_eq!((sts >> 8) & 0xffff, 14);

        let mut response = vec![0u8; 14];
        for byte in response.iter_mut() {
            *byte = read8(&mut tpm, REG_DATA_FIFO);
        }
        assert_eq!(&response[10..14], &GET_RANDOM[6..10], "echoed command code");
        assert_eq!(
            read32(&mut tpm, REG_STS) & STS_DATA_AVAILABLE,
            0,
            "all read"
        );

        // commandReady again starts the next command.
        tpm.write(0, REG_STS, &STS_COMMAND_READY.to_le_bytes());
        assert_ne!(read32(&mut tpm, REG_STS) & STS_COMMAND_READY, 0);
    }

    #[test]
    fn other_localities_and_an_inactive_locality_read_as_absent() {
        let mut tpm = TpmTis::new(Box::new(Echo));
        assert_eq!(read32(&mut tpm, 1 << LOCALITY_SHIFT), 0xffff_ffff);
        // STS reads as invalid until the locality is requested.
        assert_eq!(read32(&mut tpm, REG_STS), 0xffff_ffff);
        assert_eq!(read8(&mut tpm, REG_DATA_FIFO), 0xff);
    }

    #[test]
    fn a_failing_backend_answers_tpm_rc_failure() {
        struct Gone;
        impl TpmBackend for Gone {
            fn execute(&mut self, _: &[u8]) -> io::Result<Vec<u8>> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
        }
        let mut tpm = TpmTis::new(Box::new(Gone));
        tpm.write(0, REG_ACCESS, &[ACCESS_REQUEST_USE]);
        tpm.write(0, REG_STS, &STS_COMMAND_READY.to_le_bytes());
        tpm.write(0, REG_DATA_FIFO, &GET_RANDOM);
        tpm.write(0, REG_STS, &STS_TPM_GO.to_le_bytes());
        let response: Vec<u8> = (0..10).map(|_| read8(&mut tpm, REG_DATA_FIFO)).collect();
        assert_eq!(&response[6..10], &0x101u32.to_be_bytes());
    }

    /// Through a real swtpm, when one is installed: the control-socket
    /// handshake, then TPM2_GetRandom through the registers.
    #[test]
    fn a_command_reaches_a_real_swtpm_through_its_control_socket() {
        let Ok(dir) = tempfile::tempdir() else { return };
        let ctrl = dir.path().join("ctrl.sock");
        let Ok(mut swtpm) = std::process::Command::new("swtpm")
            .args(["socket", "--tpm2", "--tpmstate"])
            .arg(format!("dir={}", dir.path().display()))
            .arg("--ctrl")
            .arg(format!("type=unixio,path={}", ctrl.display()))
            .spawn()
        else {
            eprintln!("swtpm not installed; skipped");
            return;
        };
        for _ in 0..100 {
            if ctrl.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let backend = SwtpmSocket::connect(&ctrl).expect("handshake with swtpm");
        let mut tpm = TpmTis::new(Box::new(backend));
        tpm.write(0, REG_ACCESS, &[ACCESS_REQUEST_USE]);
        let mut run = |command: &[u8]| -> Vec<u8> {
            tpm.write(0, REG_STS, &STS_COMMAND_READY.to_le_bytes());
            tpm.write(0, REG_DATA_FIFO, command);
            tpm.write(0, REG_STS, &STS_TPM_GO.to_le_bytes());
            let size = (read32(&mut tpm, REG_STS) >> 8) & 0xffff;
            (0..size).map(|_| read8(&mut tpm, REG_DATA_FIFO)).collect()
        };
        // TPM2_Startup(SU_CLEAR), as a guest kernel sends it at boot.
        let startup = run(&[0x80, 0x01, 0, 0, 0, 12, 0, 0, 0x01, 0x44, 0, 0]);
        let response = run(&GET_RANDOM);
        let _ = swtpm.kill();
        assert_eq!(&startup[6..10], &[0, 0, 0, 0], "TPM2_Startup");
        assert_eq!(&response[6..10], &[0, 0, 0, 0], "TPM_RC_SUCCESS");
        // 10-byte header, a 2-byte size, then the 8 random bytes.
        assert_eq!(response.len(), 20);
    }
}
