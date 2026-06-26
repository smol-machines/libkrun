use std::io;
use std::path::PathBuf;

use socket2::Socket;

use super::backend::{ConnectError, NetBackend, ReadError, WriteError};
use super::sys::{self, NetRawHandle};
use super::write_virtio_net_hdr;

/// Each frame the network proxy is prepended by a 4 byte "header".
/// It is interpreted as a big-endian u32 integer and is the length of the following ethernet frame.
const FRAME_HEADER_LEN: usize = 4;

pub struct Unixstream {
    sock: Socket,
    // 0 when a frame length has not been read
    expecting_frame_length: u32,
    // 0 if last write is fully complete, otherwise the length that was written
    last_partial_write_length: usize,
}

impl Unixstream {
    /// Create the backend from an already-connected socket to the userspace
    /// network proxy. The socket is switched to nonblocking mode.
    pub fn new(sock: Socket) -> Self {
        let _ = sock.set_nonblocking(true);
        if let Err(e) = sock.set_send_buffer_size(16 * 1024 * 1024) {
            log::warn!("Failed to increase SO_SNDBUF (performance may be decreased): {e}");
        }
        log::debug!(
            "network proxy socket buffer sizes: SndBuf={:?} RcvBuf={:?}",
            sock.send_buffer_size(),
            sock.recv_buffer_size()
        );
        Self {
            sock,
            expecting_frame_length: 0,
            last_partial_write_length: 0,
        }
    }

    /// Create the backend opening a connection to the userspace network proxy.
    pub fn open(path: PathBuf) -> Result<Self, ConnectError> {
        let sock = sys::connect_unix_stream(&path).map_err(ConnectError::Binding)?;
        Ok(Self::new(sock))
    }

    /// Try to read until filling the whole slice. The socket is nonblocking; the
    /// first read (`block_until_has_data == false`) reports `NothingRead` when no
    /// data is available, and the remainder is filled by retrying on WouldBlock
    /// (the worker only calls in once the socket is readable, so this completes
    /// promptly).
    fn read_loop(&self, buf: &mut [u8], block_until_has_data: bool) -> Result<(), ReadError> {
        let mut bytes_read = 0;

        if !block_until_has_data {
            match sys::recv(&self.sock, buf) {
                Ok(0) => return Err(ReadError::Internal(io::ErrorKind::UnexpectedEof.into())),
                Ok(size) => bytes_read += size,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    return Err(ReadError::NothingRead);
                }
                Err(e) => return Err(ReadError::Internal(e)),
            }
        }

        while bytes_read < buf.len() {
            match sys::recv(&self.sock, &mut buf[bytes_read..]) {
                Ok(0) => return Err(ReadError::Internal(io::ErrorKind::UnexpectedEof.into())),
                Ok(size) => bytes_read += size,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(ReadError::Internal(e)),
            }
        }

        Ok(())
    }

    fn write_loop(&mut self, buf: &[u8]) -> Result<(), WriteError> {
        let mut bytes_send = 0;

        while bytes_send < buf.len() {
            match sys::send(&self.sock, &buf[bytes_send..]) {
                Ok(size) => bytes_send += size,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if bytes_send == 0 {
                        return Err(WriteError::NothingWritten);
                    } else {
                        log::trace!(
                            "Wrote {bytes_send} bytes, but socket blocked, will need try_finish_write() to finish"
                        );
                        self.last_partial_write_length += bytes_send;
                        return Err(WriteError::PartialWrite);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                    return Err(WriteError::ProcessNotRunning);
                }
                Err(e) => return Err(WriteError::Internal(e)),
            }
        }
        self.last_partial_write_length = 0;
        Ok(())
    }
}

impl NetBackend for Unixstream {
    /// Try to read a frame from the proxy. If no bytes are available reports ReadError::NothingRead
    fn read_frame(&mut self, buf: &mut [u8]) -> Result<usize, ReadError> {
        if self.expecting_frame_length == 0 {
            self.expecting_frame_length = {
                let mut frame_length_buf = [0u8; FRAME_HEADER_LEN];
                self.read_loop(&mut frame_length_buf, false)?;
                u32::from_be_bytes(frame_length_buf)
            };
        }

        let hdr_len = write_virtio_net_hdr(buf);
        let buf = &mut buf[hdr_len..];
        let frame_length = self.expecting_frame_length as usize;
        self.read_loop(&mut buf[..frame_length], false)?;
        self.expecting_frame_length = 0;
        log::trace!("Read eth frame from network proxy: {frame_length} bytes");
        Ok(hdr_len + frame_length)
    }

    /// Try to write a frame to the proxy.
    /// (Will mutate and override parts of buf, with a frame header!)
    ///
    /// * `hdr_len` - specifies the size of any existing headers encapsulating the ethernet frame,
    ///   (such as vnet header), that can be overwritten. Must be >= FRAME_HEADER_LEN.
    /// * `buf` - the buffer to write to the proxy, `buf[..hdr_len]` may be overwritten
    ///
    /// If this function returns WriteError::PartialWrite, you have to finish the write using
    /// try_finish_write.
    fn write_frame(&mut self, hdr_len: usize, buf: &mut [u8]) -> Result<(), WriteError> {
        if self.last_partial_write_length != 0 {
            panic!("Cannot write a frame to the proxy, while a partial write is not resolved.");
        }
        assert!(
            hdr_len >= FRAME_HEADER_LEN,
            "Not enough space to write the frame header"
        );
        assert!(buf.len() > hdr_len);
        let frame_length = buf.len() - hdr_len;

        buf[hdr_len - FRAME_HEADER_LEN..hdr_len]
            .copy_from_slice(&(frame_length as u32).to_be_bytes());

        self.write_loop(&buf[hdr_len - FRAME_HEADER_LEN..])?;
        Ok(())
    }

    fn has_unfinished_write(&self) -> bool {
        self.last_partial_write_length != 0
    }

    /// Try to finish a partial write
    ///
    /// If no partial write is required will do nothing and return Ok(())
    ///
    /// * `hdr_len` - must be the same value as passed to write_frame, that caused the partial write
    /// * `buf` - must be same buffer that was given to write_frame, that caused the partial write
    fn try_finish_write(&mut self, hdr_len: usize, buf: &[u8]) -> Result<(), WriteError> {
        if self.last_partial_write_length != 0 {
            let already_written = self.last_partial_write_length;
            log::trace!("Requested to finish partial write");
            self.write_loop(&buf[hdr_len - FRAME_HEADER_LEN + already_written..])?;
            log::debug!("Finished partial write ({already_written}bytes written before)")
        }

        Ok(())
    }

    fn raw_socket_fd(&self) -> NetRawHandle {
        sys::raw_handle(&self.sock)
    }
}
