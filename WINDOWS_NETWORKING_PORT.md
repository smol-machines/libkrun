# Windows TSI/vsock Networking Port — Implementation Plan

Status: **planning + phase 1 in progress** (2026-06-25)

Goal: bring the libkrun guest networking path (TSI — Transparent Socket Implementation,
implemented in the virtio-vsock device) up on Windows/WHP, so guest TCP/UDP/DNS reaches the
host network. Today the entire vsock subsystem is `#[cfg(not(target_os = "windows"))]`-gated
off because it is built on Unix-only primitives.

## Why this is a real port, not a cfg flip

The subsystem is ~6,700 lines across `src/devices/src/virtio/vsock/` and is coupled to Unix in
three deep ways:

1. **`nix` does not build on Windows.** It is a `target.'cfg(unix)'` dependency. Every socket
   call (`socket/bind/connect/listen/accept/recv/send/sendto/recvfrom/getpeername/shutdown/
   setsockopt`, plus `fcntl(O_NONBLOCK)`) goes through `nix::sys::socket`.
2. **The guest sockaddr ABI is expressed with `nix::SockaddrStorage`.** `packet.rs` embeds
   `SockaddrStorage` directly in the `#[repr(C)]` request/response structs (`TsiConnectReq`,
   `TsiListenReq`, `TsiGetnameRsp`, `TsiSendtoAddr`) and parses guest bytes with
   `SockaddrStorage::from_raw` (packet.rs:579). The guest sends **Linux** sockaddrs
   (AF_INET=2, AF_INET6=10); the host must translate to its own families. macOS already has
   bespoke BSD-sockaddr conversion code here — Windows needs the same treatment (AF_INET6=23).
3. **The muxer polls host sockets with epoll.** The Windows epoll shim
   (`src/utils/src/windows/epoll.rs`) is IOCP-based and currently only bridges *waitable
   handles* (EventFd). Winsock `SOCKET`s are not waitable handles and cannot be registered
   today.

A partial change leaves the tree broken or regresses the working Unix path, so this lands as
reviewable phases, each of which builds and (where possible) is verified on the host before the
next begins.

## Chosen approach

- **Socket layer: `socket2`.** It is cross-platform (Unix + Windows), exposes exactly the
  operations TSI needs (`Socket`, `SockAddr`, connect/bind/listen/accept/recv/send/send_to/
  recv_from/peer_addr/shutdown/set_nonblocking/set_reuse_address/set_keepalive), and lets the
  Unix path keep byte-identical behavior. Replaces `nix` usage in the vsock proxies.
- **Address type: a small internal `VsockAddr`** (or `socket2::SockAddr`) that (a) parses the
  Linux guest sockaddr bytes, (b) re-serializes to Linux bytes for getname responses, and (c)
  converts to/from `std::net::SocketAddr` for socket2 calls. This replaces `SockaddrStorage` in
  `packet.rs` and removes the per-OS family special-casing into one place.
- **AF_UNIX: gate out on Windows** initially (`unix.rs`, `NewProxyType::Unix`,
  `create_lisening_ipc_sockets`). Host IPC port-forward over AF_UNIX is a later, optional add
  (TCP-loopback or named-pipe backed); INET TCP/UDP is the 95% case and unblocks the e2e tests.
- **epoll: add SOCKET support to the Windows shim** via `WSAEventSelect(sock, hEvent,
  FD_READ|FD_WRITE|FD_ACCEPT|FD_CONNECT|FD_CLOSE)` bridged through the existing WCP path, with
  `WSAEnumNetworkEvents` in `wait()` translating back to `EventSet`. Additive — the EventFd
  path is unchanged.

## Phases

**Phase 1 — cross-platform socket + address layer (host-verifiable, no Windows risk).**
Add `socket2`; introduce `vsock/sys` (Socket wrapper) and `VsockAddr`; refactor
`tsi_stream.rs`, `tsi_dgram.rs`, and `packet.rs` to use them instead of `nix`. Keep Unix
behavior identical; verify with the host build + `cargo test` + clippy. Diff is large but
mechanical and fully testable on Linux/macOS.

**Phase 2 — Windows epoll SOCKET support.**
Extend `src/utils/src/windows/epoll.rs` with `WSAEventSelect`-backed socket registration and
`WSAEnumNetworkEvents` translation. Unit-test on the WHP thinkpad (cross-compiled test binary).

**Phase 3 — wire vsock into the Windows build.**
Gate `unix.rs`/`NewProxyType::Unix`/`create_lisening_ipc_sockets` behind `cfg(unix)`; drop the
`cfg(not(windows))` gates on the vsock device, `krun_add_vsock`, and `krun_set_port_map` in
`src/libkrun/src/lib.rs` and `src/vmm`. Make `nix` no longer required by the vsock path. Get a
clean Windows `krun.dll` with the device instantiating.

**Phase 4 — bring-up + test on WHP hardware.**
Build with TSI, deploy, validate outbound TCP (e.g. `wget http://…`), DNS (the device has a
`dns_filter`), and UDP from the guest. Fix the inevitable Winsock error-mapping and
nonblocking-connect (`WSAEWOULDBLOCK` vs `EINPROGRESS`) edge cases.

**Phase 5 (optional) — inbound port-forward + AF_UNIX host IPC.**
`accept()` reverse proxies and a Windows replacement for the AF_UNIX `unix_ipc_port_map`.

## Key risks / watch-items

- **Guest ABI fidelity**: getname responses must serialize *Linux* sockaddr bytes regardless of
  host — the conversion must be exact (port byte-order, `sin6` fields). This is the highest-risk
  area; cover it with round-trip unit tests in `packet.rs`.
- **Nonblocking connect**: Windows reports in-progress connect as `WSAEWOULDBLOCK`, not
  `EINPROGRESS`; the `connect()` proxy path must treat both as "Connecting".
- **`SO_REUSEPORT`**: no Windows equivalent — map to `SO_REUSEADDR`.
- **epoll Delete/GC race** already documented in the shim applies to sockets too; reuse the
  zombie-list pattern.
- Do not regress the Unix path: every phase must keep `cargo test` green on Linux/macOS.
