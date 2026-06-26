# Windows TSI/vsock Networking Port — Implementation Plan

Status: **COMPLETE — hardware-validated** (2026-06-26)

Guest networking works on the Windows Hypervisor Platform. Validated on the
Windows 10 1909 WHP thinkpad: the guest's outbound **TCP** (a `nc` HTTP GET to
`1.1.1.1:80` returned `HTTP/1.1 301 ... Server: cloudflare`, and a `wget` to
`:443` completed a full TLS handshake receiving the server certificate) and
**UDP/DNS** (`nslookup example.com 1.1.1.1` resolved to real A records) both go
through the Windows socket2/Winsock TSI backend. All four phases done.

## Progress log

- ✅ **Phase 1 — socket2 + VsockAddr** (`884d043`, `5f0b483`): the whole TSI proxy
  set (`tsi_stream`, `tsi_dgram`, `packet`, `muxer`, `dns_filter`, `proxy`) is off
  `nix` and on `socket2` + a host-neutral `VsockAddr`. `unix.rs` (AF_UNIX host
  IPC) stays nix-based on Unix. Unix behavior unchanged; 78 devices tests pass.
- ✅ **Phase 2 — Windows epoll SOCKET support** (`9e76c3e`): `utils/windows/epoll.rs`
  bridges Winsock sockets into the IOCP via `WSAEventSelect` + `WSAEnumNetworkEvents`.
  Cross-compiles for `x86_64-pc-windows-gnu`.
- ✅ **Phase 3 (1/2) — cross-platform proxy handle** (`3fe9fa4`): `Proxy::poll_handle()`
  + `ProxyRawHandle` replace the Unix-only `AsRawFd` supertrait.
- ⏳ **Phase 3 (2/2) — remaining build wiring.** Next concrete steps:
  1. `vsock/mod.rs`: gate `mod unix;` behind `#[cfg(unix)]`.
  2. `muxer.rs` / `muxer_thread.rs`: gate the AF_UNIX bits for Windows — the
     `UnixProxy` import + construction, `unix_ipc_port_map`, and
     `create_lisening_ipc_sockets`; change `update_polling`'s `RawFd` param to
     `ProxyRawHandle`.
  3. `devices/src/virtio/mod.rs`: drop the `#[cfg(not(target_os = "windows"))]`
     gates on `pub mod vsock` / `pub use vsock::*`.
  4. `libkrun/src/lib.rs` + `vmm`: drop the `#[cfg(not(windows))]` gates on the
     vsock device, `krun_add_vsock`, `krun_add_vsock_port*`, and `krun_set_port_map`.
  5. Cross-compile `krun.dll` with vsock; fix residual Windows-only errors.
- ✅ **Phase 3 (2/2) — build wiring** (`f35422a`, `ea21344`, `a318e3d`, `95d55ac`):
  AF_UNIX gated to Unix throughout (muxer/muxer_thread/libkrun); `update_polling`
  takes `ProxyRawHandle`; EventFd epoll registration uses the platform `AsRawFd`;
  `libc::AF_*`/`SOMAXCONN`/`O_NONBLOCK` replaced with the guest's Linux constants;
  the IOCP `Epoll` marked `Send`; and the `cfg(not(windows))` gates dropped on
  `pub mod vsock`, the vmm vsock device/config/builder, and the libkrun
  `krun_add_vsock`/`krun_set_port_map`/`krun_add_vsock_port` C API. `krun.dll`
  builds for `x86_64-pc-windows-gnu` with TSI.
- ✅ **Phase 4 — hardware bring-up**: outbound TCP + UDP/DNS validated on WHP
  (see status above). The vsock event-handler WouldBlock log noise was silenced
  to match the other devices.

The port is functional. Possible follow-ups (not blocking): inbound
port-forward (`accept()` reverse proxies) and AF_UNIX host IPC on Windows are
still Unix-only; the WSAEventSelect level-vs-edge semantics could use a
soak test under heavy concurrent connections.


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

## Validation battery (2026-06-26)

A 15-test PASS/FAIL battery driven by a single launcher (virtiofs + virtio-blk +
vsock) passed **15/15** on the WHP thinkpad: process/exec (echo, pipe, exit
codes, nested exec), virtiofs (write/read/append/delete/mkdir/1 MB file),
virtio-blk (`/dev/vda` write+read), shell (loop, conditional), and networking
(DNS over UDP, outbound TCP, and an **8-way concurrent-connection soak** that
exercises the `WSAEventSelect` path under load).

**Inbound port-forward** (host→guest via `krun_set_port_map` + the `accept()`
reverse proxy) is validated: a host TCP client reached a guest listener through
the mapped port and received its response. A dual-stack fix (`set_only_v6(false)`
on IPv6 sockets) was added so an IPv6 wildcard listener also accepts IPv4,
matching Linux's default — verified working over both IPv4 and IPv6.

Remaining genuinely-unsupported (not bugs): **AF_UNIX host-IPC over TSI** is
Unix-only — Windows has no AF_UNIX, so `krun_add_vsock_port` (the UDS bridge)
is a no-op there; INET TCP/UDP and port-forward are unaffected.
