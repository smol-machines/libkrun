# Windows (WHP) Support Plan for libkrun

Status: **draft / scoping** — written 2026-06-24 after a build-driven assessment on the
`windows-whp-fixes` branch (cross-compiling from macOS to `x86_64-pc-windows-gnu`).

## VALIDATED ON REAL HARDWARE (2026-06-25)

The WHP backend was **booted on a real Windows host** (AWS `z1d.metal`, Windows Server
2022, Windows Hypervisor Platform enabled). Built the kernel + `libkrunfw.dll` + `krun.dll`
+ a launcher on a Linux instance, deployed via SSM/S3, and ran it. Result:

- ✅ **WHP partition created, guest memory mapped, vCPU created** — all real WHP API calls.
- ✅ **The Linux kernel boots**: runs at kernel virtual addresses (long mode + paging +
  segments programmed correctly), executes PIO (PIT, CMOS/RTC, port 0x80), and **MMIO
  instruction emulation drives the IOAPIC and virtio-mmio** via the `WHV_EMULATOR_CALLBACKS`
  bridge.
- ✅ **Interrupts deliver**: the per-device watcher → IOAPIC → `request_interrupt` path
  works (balloon free-page reporting runs).
- ✅ **virtio devices initialize and process I/O**: balloon, rng, and **virtio-fs reaches
  FUSE_INIT** (the guest is mounting the rootfs).

Two bugs found and fixed by hardware debugging (commit `f972d32`):
1. MSR **writes** never advanced RIP → the guest's `WRMSR` to Hyper-V synthetic MSRs
   (`0x4000_00xx`) looped forever. Fixed.
2. `WHvSetVirtualProcessorRegisters` **hangs** on the SYSENTER/STAR/TSC MSRs; worked around
   by setting only `MsrMtrrDefType` (the kernel re-initializes the rest). Root-cause TODO.

Three bugs found and fixed by hardware debugging (commits `f972d32`, `7a3daec`, `386e3a1`):
1. MSR **writes** never advanced RIP → the guest's `WRMSR` to Hyper-V synthetic MSRs looped
   forever. Fixed.
2. WHP IOAPIC set `REMOTE_IRR` for level-triggered lines and waited for an EOI that WHP
   handles natively and never reports → re-injection blocked after the first interrupt.
   Fixed (inject once per device assertion).
3. `WHvSetVirtualProcessorRegisters` intermittently **hangs** programming MSRs at boot
   (timing/host-dependent). The pre-boot MSR programming is now skipped (the kernel
   re-initializes those MSRs), so the guest **reliably boots through device init**.

### Third hardware-validation pass (2026-06-25): virtio-fs stall root-caused and fixed

The "stalls at FUSE_INIT" gap above was chased to ground on real WHP hardware and resolved
with three further fixes. The guest now boots **through kernel init into the init-exec
sequence**, mounting and traversing its virtiofs root:

- ✅ `FS-LOOKUP parent=1 name="dev"`, then `FS-LOOKUP parent=1 name="init.krun"` — the guest
  kernel mounts the virtiofs root and looks up `/init.krun` (the libkrun PID-1 binary). Full
  opcode flow observed: `FUSE_INIT(26) → GETATTR(3) → LOOKUP(1) → OPEN(14) → READ(15)`.
- ✅ The VM **powers off with a clean exit code 0** (orderly HLT shutdown, not a watchdog
  kill or panic-reboot loop).

Root cause of the original FUSE_INIT stall was **two stacked WHP interrupt bugs**, not a FUSE
issue (the balloon "interrupts work" signal was misleading — balloon free-page reporting is a
guest kernel thread that runs without needing a completion IRQ):

1. **The software IOAPIC was never on the MMIO bus** (commit `c9260fe`).
   `attach_legacy_devices` only registered the IOAPIC window under `split_irqchip` (default
   off). WHP does not emulate the IOAPIC, so the guest's writes to `0xFEC00000` to program the
   redirection table were silently dropped → every virtio pin stayed masked at reset → no
   device-completion interrupt was ever delivered → the guest hung waiting for the FUSE_INIT
   reply. Now always registered on Windows.
2. **Interrupts were injected level-triggered** (commit `b4d800f`). With the IOAPIC reachable,
   `WHvRequestInterrupt(Level)` told WHP the line was held high with no de-assertion the
   software IOAPIC could provide → the LAPIC re-fired after every EOI → an interrupt storm
   that pinned a host core (and starved the SSM agent during debugging). The backend's
   once-per-assertion model requires an **edge** pulse; switched to `InterruptTriggerMode::Edge`.

A third, latent bug was fixed along the way (commit `8c9b173`): the Windows epoll shim's
`NtAssociateWaitCompletionPacket` re-arm queued no packet when the target was already signaled
(it sets `AlreadySignaled` and consumes the wait as a one-shot), so a level-triggered fd fired
exactly once — the device worker got FUSE_INIT but never woke for the next request. Now posts
the completion packet itself, matching Linux level-triggered semantics.

### Fourth pass (2026-06-25): the guest reaches userspace and runs PID-1 init

The "halts after looking up /init.krun" gap was a **wrong-architecture init binary**, not an
AugmentFs bug. `init_blob/build.rs` builds `krun-init` for the guest arch (= the libkrun
target arch); when cross-compiling libkrun for an **x86_64** guest from an **aarch64** macOS
host, the `x86_64-unknown-linux-musl` target was not installed and its cross-linker was not
configured, so the build silently fell back to a host-arch (aarch64 Mach-O) `init.krun`. The
x86_64 guest looked it up, could not exec the wrong-arch binary, and halted. Fixed by
installing the target and adding the x86_64 musl linker recipe to the Makefile (commit
`83a448d`); the embedded init is now a proper `ELF x86-64 static-pie` binary.

A second issue surfaced and was fixed: the WCP "post on already-signaled" from `8c9b173`
**double-delivered** every event (the kernel re-queues natively), flooding the virtio device
event handlers with spurious `WouldBlock` wakeups and starving the host. Reverted (`aef55b7`);
the original one-shot symptom was the interrupt bug, already fixed.

With the correct init and a console-enabled launcher, the FS-LOOKUP trace shows a **real
userspace boot** on real WHP hardware:

```
dev → init.krun                 (kernel exec's PID 1)
dev, proc, sys                  (init.krun mounts the pseudo-filesystems)
.krun_config.json               (init reads its config — it is running)
bin → sh → busybox              (init exec's the workload shell)
lib → ld-musl-x86_64.so.1       (loading busybox's dynamic linker)
```

**This is a Linux guest booting to userspace under the Windows Hypervisor Platform**: kernel
init completes, the virtiofs root mounts, PID-1 `init.krun` runs, sets up `/dev`,`/proc`,`/sys`,
reads `.krun_config.json`, and exec's the busybox shell with its dynamic loader.

**Remaining final-mile** (scoped, beyond the hypervisor/interrupt/init core, all now working):

1. **busybox/ld-musl load completes.** The exec of the dynamically-linked busybox stops at
   loading `ld-musl-x86_64.so.1` (process exits 126, "cannot execute"). The ELF header read
   works (the guest found `PT_INTERP` and looked up the loader); the remaining failure is in
   the final loader/relocation step — likely a virtiofs passthrough read-correctness detail
   for real (non-virtual) files on Windows, or visible only with kernel-console output.
2. **virtio-console TX→host output.** Even with a console *device* configured (the stock test
   launcher omits `krun_add_virtio_console_default`), guest `console=hvc0` output is not
   delivered to the host log/stdout, so kernel/shell output (and the `BOOT_OK` marker) stays
   invisible — which also blocks diagnosing #1 from inside the guest.

**Bottom line: the WHP backend is functional through userspace init on real hardware.** From
this session's start (stalled at FUSE_INIT, no filesystem traversal, no userspace) the guest
now boots the kernel, mounts root, runs PID-1 init, and exec's the userspace shell. The two
remaining items are a static-vs-dynamic binary load detail and console output visibility —
neither in the hypervisor core.

### Fifth pass (2026-06-25): older WHP (Windows 10 1909) bring-up

Tested on a real Windows 10 Pro 1909 laptop (Intel i7-9700, Hyper-V enabled, reached over
Tailscale SSH — no cloud). That WHP build predates several APIs the backend used, which
aborted VM creation until worked around (commits `ee49a71`, `14eb29b`):

- **Unsupported partition properties/capabilities.** `X64MsrExitBitmap` (property 5),
  `ProcessorFeaturesBanks` (capability + property) and `SyntheticProcessorFeaturesBanks`
  (property 0x100c) all return `WHV_E_UNKNOWN_PROPERTY/CAPABILITY` on 1909. Now skipped
  best-effort; the invariant-TSC CPUID leaf is advertised unconditionally instead of via the
  missing banks property.
- **HLT surfaces as a run-loop exit.** Newer WHP services idle HLTs inside
  `WHvRunVirtualProcessor`; 1909 returns a Halt exit, so the vCPU must re-enter on an idle
  (interrupts-enabled) HLT and only stop on a masked HLT.

With those, the guest now boots through kernel init on 1909: maps memory, runs the vCPU,
reads the RTC, and programs the IOAPIC redirection entries for the virtio devices
(balloon/rng/console/fs). It then hits a **silent early triple fault**
(`WHvRunVpExitReasonUnrecoverableException`, or an i8042 reboot via `panic=-1`) ~1.3 s in,
right after the virtio IOAPIC setup — before the virtio drivers activate. It is
timing-dependent (manifests as triple-fault or reboot depending on host load).

Diagnosis is blocked by the lack of an early console: the libkrunfw microVM kernel has **no
8250/serial driver** (verified: `console=ttyS0 earlyprintk=ttyS0` produces zero writes to
0x3f8), and the only console it supports — virtio `hvc0` — is not up yet when it faults. The
next step is to trap the first exception via `WHvPartitionPropertyCodeExceptionExitBitmap`
(if 1909 supports it) to recover the faulting vector/error-code/RIP, since the triple fault
itself is silent.

### Sixth pass (2026-06-25): FULL USERSPACE SHELL on Windows 10 1909

The early 1909 fault was the kernel's `check_timer` panic ("IO-APIC + timer doesn't work!");
`no_timer_check` skips it (the WHP-emulated LAPIC timer works). Past that, two filesystem
bugs blocked userspace, both fixed:

- **Passthrough files were non-executable.** Windows has no Unix execute bit, so every file
  was reported `0o644`; `execve` failed with EACCES (exit 126). Default to `0o755`.
- **Short reads corrupted demand-paged binaries.** The Windows positioned read returned fewer
  bytes than requested and the default vectored read filled only the first buffer, so mmap'd
  pages of the dynamic loader were partly zero-filled → SIGSEGV (exit 139). Static binaries
  (init) were fine; dynamic ones (`/bin/sh` + ld-musl) crashed. Loop the read to full length.

With these, on a real **Windows 10 Pro 1909** laptop (i7-9700, reached over Tailscale SSH, no
cloud) the guest **boots all the way to a userspace shell**, reproducibly (3/3, clean exit 0):

```
===BOOT_OK_FROM_GUEST=== shell-is-alive-on-WHP
```

i.e. kernel boot → mount virtiofs root → PID-1 init → exec `/bin/sh` (busybox + ld-musl) → the
shell runs and writes to the host console, then powers off.

**Status: the Windows Hypervisor Platform backend boots a Linux guest to a working userspace
shell, validated on real hardware (Windows 10 1909, and through userspace init on Server
2022).** Minor remaining items: busybox applet re-exec via `/proc/self/exe` fails when init
launched the shell from a memfd (a fast-path artifact, not a boot failure), and the
virtio-console kernel-message stream isn't drained to the host log (the workload's stdout via
the krun-stdout port works). The chain of fixes — IOAPIC-over-MMIO, edge IRQ injection,
guest-arch init, WCP re-arm, older-WHP partition/HLT tolerance, `no_timer_check`, executable
mode, full-length reads — is what carries the guest from "won't create a partition" to a live
shell.

## TL;DR

**Update (2026-06-25): the entire libkrun stack now cross-compiles to a clean `krun.dll`
(PE32+ Windows DLL, 0 code warnings) from macOS** — `cargo build --target
x86_64-pc-windows-gnu -p libkrun`. The WHP VMM integration (vstate run loop, MMIO device
manager, builder VM-assembly, vCPU creation) was written and compiles; the device layer,
C deps (bzip2/zstd via mingw), and the C-API layer all build; host macOS/Linux builds stay
green. **What remains for *functional* is runtime: booting a Linux guest under WHP on a
real Windows host** — the `TODO(whp-host)` items (boot register/segment programming, MMIO
instruction emulation, IRQ injection wiring) need that hardware to validate, plus a Windows
kernel blob from libkrunfw. See the implementation log below for the full breakdown.

The original assessment (kept for history): the upstream sync brought in a real WHP backend;
the hypervisor core compiled, but integration glue, one non-portable dependency, the
build/test plumbing, and smolvm's Unix-only features were missing.

## Implementation log (2026-06-25)

Progress made on branch `windows-whp-fixes`:

- ✅ **`krun-utils` Windows edition fix** — `unsafe extern` in `bindings.rs`; the crate now
  cross-compiles for `x86_64-pc-windows-gnu`.
- ✅ **Dependency wall cleared (the hard one).** The `vm-memory` "rawfd feature is not
  supported on Windows" error was *not* caused by `imago` (its `vm-memory` dep is already
  `default-features = false`). The sole source was **`linux-loader 0.13.2`**, which declares
  `vm-memory` with default features; Cargo's global per-target feature unification then
  forced `rawfd` on for the whole Windows graph. Fixed by vendoring `linux-loader` at
  `vendor/linux-loader` with a one-line `default-features = false` and a
  `[patch.crates-io]` entry in the root `Cargo.toml`. `cargo tree` confirms `rawfd` is gone
  from the Windows graph. *(Upstream fix worth filing: rust-vmm/linux-loader should set
  `default-features = false` on its `vm-memory` dependency.)*
- 🔴 **Discovered the real blocker: there is no VMM↔WHP integration.** Past the dependency
  wall, the first code errors are KVM couplings (e.g. `krun-cpuid` `use kvm_bindings::CpuId`)
  that need `cfg`-gating — mechanical. But the structural finding is bigger: **`vmm/src/`
  has `linux/` and `macos/` platform modules (the vCPU run loops) but no `windows/` module,
  and the `whp` crate is never referenced by `vmm`.** Upstream shipped the low-level WHP
  wrappers (`src/whp/` ~1077 LoC, `src/arch/src/x86_64/windows/` ~322 LoC) but never wrote
  the layer that actually runs a VM on them. For comparison the equivalent integration is
  `macos/vstate.rs` ~983 LoC and `linux/vstate.rs` ~2360 LoC. So Windows needs ~800–1000
  LoC of **net-new** VMM code: partition setup, guest-memory mapping, vCPU threads, the WHP
  exit-handling run loop (cpuid/msr/io/mmio/halt), and IRQ injection.

### Progress since (same session, continued)

- ✅ **rawfd correctly scoped (not globally disabled).** The first cut of the
  linux-loader patch removed `vm-memory`'s `rawfd` from *every* target and broke the
  Linux/macOS build (`vm-memory`'s `rawfd` feature is what provides
  `read_volatile`/`write_volatile` on file descriptors, used by the console port I/O).
  Fixed by explicitly enabling `vm-memory = { features = ["backend-mmap", "rawfd"] }`
  under `devices`' `[target.'cfg(unix)'.dependencies]`. Net effect: rawfd **on** for
  Unix, **off** for Windows. Host build verified clean (`cargo check -p krun-vmm` on
  macOS), Windows graph verified rawfd-free (`cargo tree`).
- ✅ **`whp` wired into the VMM.** `krun-whp` added to `vmm`'s Windows deps;
  `vmm/src/lib.rs` now selects `windows::vstate` under `cfg(windows)` alongside
  `linux`/`macos`.
- ✅ **`krun-cpuid` gated to Linux-x86_64** in `vmm/Cargo.toml` (it is built on
  `kvm_bindings`; WHP does its own CPUID).
- ✅ **WHP vstate scaffold written** — `vmm/src/windows/{mod,vstate}.rs`. Provides the
  `Vm`/`Vcpu`/`VcpuHandle`/`VcpuEvent`/`VcpuResponse`/`VcpuConfig`/`VmState`/`VcpuState`
  surface the rest of the VMM consumes, backed by `WhpVm`/`WhpVcpu`: partition creation,
  guest-memory mapping (`map_memory` over each region), a vCPU thread, and the WHP
  exit-handling run loop (PIO→`io_bus`, MMIO→`mmio_bus`, CPUID/MSR completion, Halt).
  Sites needing real-hardware validation (boot register/segment programming, MMIO
  instruction emulation via `WhpEmulator`, prompt vCPU cancellation on events) are marked
  `TODO(whp-host)`. **Not yet compile-verified for Windows** — see device-layer blockers
  below — but it does not affect the (verified-clean) Linux/macOS builds.

### Device layer now compiles for Windows ✅ (continued session)

`cargo check --target x86_64-pc-windows-gnu -p krun-devices` went from **111 errors → 0**.
What it took (all host builds stay green — verified `cargo check` on macOS for
`krun-utils`/`krun-devices`/`krun-vmm` and `libkrun --no-default-features`):
- `linux_errno.rs` — gated the 11 Unix-only errno match arms (`ENOTBLK`, `ESHUTDOWN`, …)
  with `#[cfg(not(windows))]`.
- `file_traits.rs` — gated the libc/`AsRawFd` `volatile_impl` macro to non-Windows and added
  a Windows `File` impl of `FileReadWriteVolatile`/`FileReadWriteAtVolatile` using
  `std::os::windows::fs::FileExt` (`seek_read`/`seek_write`) + `Read`/`Write`.
- `bindings.rs` — added a Windows arm defining `stat64`/`statvfs64` (field names/types
  matched to what `fs/windows/passthrough.rs` constructs and `fuse::Attr`/`Kstatfs` read) plus
  `off64_t`/`ino64_t` aliases.
- `fs/windows/passthrough.rs` — added the missing `Wdk`/`Win32_System_*` windows-sys features
  (`Wdk_Foundation`, `Wdk_Storage_FileSystem`, `Win32_System_IO`/`SystemInformation`); added
  `snapshot`/`restore` stubs to the Windows `PassthroughFs` (snapshot/fork is Unix-only for now).
- `vsock` (TSI over Unix sockets, ~53 errors) — gated the module off Windows
  (`#[cfg(not(target_os = "windows"))] pub mod vsock`) and its `persist.rs` snapshot arms.
- `rng/event_handler.rs`, `legacy/x86_64/serial.rs` — switched the `AsRawFd` import to the
  `utils::windows::AsRawFd` shim under `cfg(windows)`; added the `File` impl of that shim trait
  in `utils` (orphan-rule: it must live where the trait is defined) and an `AsRawFd` supertrait
  on the Windows `ReadableFd`.

The WHP **vstate scaffold now type-checks** against the (now-compiling) `devices` + `whp`
crates — 0 errors attributed to `vmm/src/windows/vstate.rs` (needed `vm_memory::{Address,
GuestMemoryRegion}` in scope).

### Remaining: `krun-vmm` Windows compile (64 → 59 errors)

The leftover errors are the deepest integration layer:
- **`builder.rs` (~38)** — the microVM assembly is KVM/x86-PIC/vsock-shaped: `vm.fd()`,
  `supported_cpuid`/`supported_msrs`, `CreateKvmIrqChip`, `KvmIoapic`/`IoApic`, `Vsock`/`TsiFlags`,
  `WorkerMessage::{IrqLine,GsiRoute,ConvertMemory}`, `kvm_bindings`. Needs `cfg(linux)` gating +
  WHP equivalents (interrupt routing via `WhpVm::request_interrupt`).
- **`device_manager` Windows MMIO submodule (net-new)** — `device_manager::mmio` is re-exported
  from `kvm/` (Linux) or `hvf/` (macOS); Windows has neither. A `whp/mmio.rs` (~hundreds of LoC)
  is needed — the macOS one is aarch64/GIC-coupled and the KVM one is irqfd-coupled, so neither
  copies cleanly.
- `lib.rs` (~10), `worker.rs` (~6), `resources.rs`/`vmm_config/vsock.rs` (~4) — mostly
  `AsRawFd`/`errno`/vsock-config gating (tractable), plus signal-handling (`register_kick_signal_handler`).

This is net-new WHP integration that needs a Windows+WHP host to validate behaviour, so it is the
natural boundary for compile-from-macOS progress.

### ✅ Full Windows cross-compile achieved (2026-06-25)

`cargo build --target x86_64-pc-windows-gnu -p libkrun` now links a clean
`target/x86_64-pc-windows-gnu/debug/krun.dll` (PE32+ DLL, import lib `libkrun.dll.a`,
**0 code warnings**). Host macOS dylib build verified unaffected (`install_name`
`libkrun.2.dylib` intact). What it took, by crate:

- **`krun-vmm` (64 → 0):**
  - **Net-new `device_manager/whp/mmio.rs`** — WHP MMIO device manager (no irqfd/ioeventfd;
    WHP traps MMIO in the run loop, IRQs via `WhpVm::request_interrupt` — `TODO(whp-host)`).
  - **Net-new WHP build path in `builder.rs`** — a Windows-x86_64 block (WHP IOAPIC +
    `create_vcpus_x86_64_whp`), a Windows `setup_vm` (passes vcpu_count, which WHP needs at
    partition-create time), and the WHP `Vm`/`Vcpu` wiring.
  - Gated KVM-only paths to `cfg(linux)`: irqfd legacy-device registration, `vm.fd()`,
    `supported_cpuid`/`supported_msrs`, `KvmIoapic`/`IoApic`, `WorkerMessage::{GsiRoute,IrqLine}`.
  - Gated the vsock/TSI cascade off Windows (`vmm_config::vsock`, `resources::{VsockConfig,
    VsockBuilder,TsiFlags,set_vsock_device}`, builder attach). Added `console_output` /
    `disable_implicit_console` `VmResources` fields the Windows console scaffold needed.
  - Windows page size via `GetSystemInfo`; `DEFAULT_KERNEL_CMDLINE` Windows arm; kernel
    bundle mapped with a fresh `MmapRegion` + copy (no `build_raw` on Windows); external ELF
    load returns unsupported on Windows (`File: ReadVolatile` is Unix-only in vm-memory).
- **`krun-utils`:** added a cross-platform `errno` shim (`utils::windows::errno`, since
  `vmm-sys-util` is Unix-only) and an `AsRawFd for File` impl for the Windows shim.
- **`libkrun` (C API):** gated the Unix-only surface off Windows — vsock C functions
  (`krun_add_vsock` returns `ENOTSUP`), control socket (`UnixListener`/`UnixStream`),
  privilege drop (`setuid`/`setgid`), `mmap` (read-into-leaked-buffer fallback for external
  kernels), `krun_init_log` FD-pipe arm; `uid_t`/`gid_t` → `u32`; `KRUNFW_NAME` = `krunfw.dll`.
- **Build plumbing:** fixed `src/libkrun/build.rs` to select soname/install_name link args by
  `CARGO_CFG_TARGET_OS` (the *target*) instead of `#[cfg(target_os)]` (the *host*) — the latter
  emitted macOS `-install_name` for the Windows link and broke it. This also corrects native
  cross-builds generally.

### WHP run loop: what's now implemented vs. still host-dependent

Implemented for real (compiles; behaviour needs a WHP host to confirm):
- ✅ **Boot register/segment/MSR programming** — `Vcpu::configure_x86_64` now calls the
  existing WHP arch backend (`arch::x86_64::windows::{msr::setup_msrs, regs::setup_sregs,
  regs::setup_regs}`) in the KVM/HVF order (MSRs → sregs/segments/page-tables → GPRs+RIP).
- ✅ **PIO (`in`/`out`)** — the run loop decodes the WHP IO-port exit (direction + 1/2/4-byte
  size via the new `WhpVcpu::io_port_exit_info`), routes to `io_bus`, and writes the `in`
  result back to RAX (`WhpVcpu::complete_io_in`).
- ✅ **CPUID / MSR exits** — completed via `complete_cpuid` / `complete_msr_read`.
- ✅ **IOAPIC → guest interrupt injection** — already done by `WhpIoapic::service`
  (`WhpVm::request_interrupt`), wired onto the MMIO bus via `register_mmio_ioapic`.

Now also implemented (compile-verified; behaviour still needs a WHP host to confirm):
- ✅ **MMIO instruction emulation** — added a full `WHV_EMULATOR_CALLBACKS` bridge in the
  `whp` crate (`WhpEmulator::with_mmio_callbacks` + `emulate_mmio`): the memory callback routes
  device-side accesses to a closure (wired to the MMIO bus in `vstate`), and the
  register/translation callbacks forward to `WHvGet/SetVirtualProcessorRegisters` /
  `WHvTranslateGva`. The `MemoryAccess` arm now drives the emulator (which also advances RIP).
- ✅ **Device `interrupt_evt` → IOAPIC wiring** — `builder::attach_mmio_device` spawns a
  per-device watcher thread that waits on the device's interrupt EventFd and raises the IOAPIC
  line (`IrqChipDevice::set_irq` → `WhpVm::request_interrupt`), the WHP analogue of KVM's irqfd.
- ✅ **Prompt vCPU cancellation** — `VcpuHandle::send_event` now calls `WhpVm::cancel_vcpu` to
  kick the vCPU out of `WHvRunVirtualProcessor`.

Remaining `TODO(whp-host)` are now refinements, not blockers: CPUID masking/templating, MSR
specifics, snapshot/fork register capture, a zero-copy kernel-bundle view, legacy COM/keyboard
IRQ lines, and a Windows HANDLE log-pipe target.

### Still required for a *running* Windows VM
- A `Makefile` Windows target (today `krun.dll` is produced by a direct
  `cargo build --target x86_64-pc-windows-gnu -p libkrun`; the Makefile keys off `uname -s`
  and needs MSYS2/mingw-aware OS detection — native-Windows-only, untestable from macOS).
- A Windows-loadable kernel blob from **libkrunfw** (`krunfw.dll` or embedded bytes).
- A Windows host with the Windows Hypervisor Platform enabled to build natively (MSVC) and
  runtime-debug the first boot — the WHP run loop is implemented but its guest-visible
  behaviour (boot programming, MMIO emulation, IRQ delivery) can only be validated there.
- Optional: re-home smolvm's Unix-only features (TSI networking, control socket, snapshot/fork).

### (historical) device-layer inventory before the fixes above

`vmm` cannot compile for Windows until `devices` does. Current Windows error inventory
(`cargo check --target x86_64-pc-windows-gnu`): `krun-devices` ~111, `krun-vmm` ~118.
The device-layer errors cluster as:
- `virtio/bindings.rs` (~80) — libc file-I/O types/functions (`stat64`, `off64_t`,
  `pread64`/`preadv64`, …) have Linux/macOS arms but no Windows arm; underpin virtio-fs.
- `vsock/*` (~40) — TSI host networking built on Unix domain sockets (`OwnedFd`, …).
- `virtio/linux_errno.rs`, `virtio/file_traits.rs` — Unix errno + fd vectored I/O.
- `fs/*` — virtio-fs passthrough (Linux FUSE-protocol passthrough via libc `openat`/`statx`).

These are genuine per-subsystem ports (virtio-fs passthrough to Win32 file APIs is the
largest), or must be `cfg`-gated off Windows for an initial minimal build — but virtio-fs
is on the boot path (init reads `.krun_config.json` from the virtiofs root), so gating it
trades away a runnable config. After `devices`, `vmm` needs its remaining KVM call sites
(`builder.rs`, cpuid usage) gated, then `libkrun` itself.

### Why "functional" can't be finished in this environment
- The missing WHP run loop is net-new code that must be **debugged against a real Windows
  host with the Windows Hypervisor Platform enabled** — cross-compilation from macOS can
  only prove it *compiles*, never that it *runs*. No such host is available here.
- Writing ~1000 LoC of WHP run-loop logic with no runtime to test against would be
  low-confidence. The compile-gating work (KVM `cfg`s) only pays off *after* that module
  exists, since `vmm` cannot compile for Windows without a `windows` vstate to satisfy
  `lib.rs`.

## What already works (verified)

Cross-compiled `cargo check --target x86_64-pc-windows-gnu` per-crate:

| Crate | Result | Notes |
|---|---|---|
| `krun-whp` | ✅ Finished | WHP partition / vCPU / memory-mapping wrappers |
| `krun-arch` (x86_64 Windows) | ✅ Finished | register/MSR/IO-APIC backend (`src/arch/src/x86_64/windows/`) |

Windows source already present from the sync (scaffolding to build on):
- `src/whp/` — WHP bindings crate
- `src/arch/src/x86_64/windows/{mod,msr,regs}.rs`
- `src/devices/src/legacy/ioapic_whp.rs`
- `src/devices/src/virtio/console/port_io/windows.rs`
- `src/devices/src/virtio/fs/windows/{fs_utils,mod,passthrough}.rs`
- `src/utils/src/windows/{bindings,epoll,eventfd,mod}.rs`

## Blockers (in dependency order — each gates the next)

### 1. `krun-utils` — Rust 2024 edition cleanup  ·  effort: S (~mechanical)
- `extern` blocks must be `unsafe extern` under edition 2024.
- **Done:** `src/utils/src/windows/bindings.rs` (`ntdll` block).
- **TODO:** sweep the rest of the Windows modules (`epoll.rs`, `eventfd.rs`) for the
  same edition issues (`#[no_mangle]` → `#[unsafe(no_mangle)]`, `unsafe extern`, explicit
  `unsafe {}` blocks).

### 2. `imago` (COW disk library) does not support Windows  ·  effort: L  ·  **structural, highest risk**
- `krun-devices` depends on `imago` 0.2.3 with its `vm-memory` feature.
- `imago` pulls `vm-memory` with default features, which includes **`rawfd`** — and
  `vm-memory` hard-errors: `rawfd feature is not supported on Windows targets!`
  (21 downstream errors). This is a *graph-resolution* failure, before any of our code
  compiles.
- This is the real wall. Options, roughly in order of preference:
  1. **Make block-COW optional on Windows** — gate the `imago` dependency and the
     `imago`-using block code (`src/devices/src/virtio/block/...`) behind
     `#[cfg(not(target_os = "windows"))]`. First Windows builds ship without COW disk
     support (raw disks only, or no block device initially).
  2. Get a Windows-capable `imago` (upstream the fix, or fork) so its `vm-memory`
     integration doesn't require `rawfd`.
  3. Drop `imago`'s `vm-memory` feature and reimplement the guest-memory bridge the block
     device needs in a portable way.
- **Recommendation:** start with (1) to unblock the rest of the build, file an upstream
  issue for (2).

### 3. C dependencies need a Windows toolchain  ·  effort: S–M
- `src/vmm/Cargo.toml` pulls `bzip2` (→ `bzip2-sys`) and `zstd` (→ `zstd-sys`)
  **unconditionally** for kernel-blob decompression — both compile C.
- Cross-compiling to `*-windows-msvc` from macOS is not viable; `*-windows-gnu` + mingw-w64
  works (mingw is installed locally and got past these once the graph resolves).
- **TODO:** either (a) build natively on Windows with MSVC + the C deps' build prereqs, or
  (b) keep using the gnu toolchain for CI cross-builds. Confirm `zstd`/`bzip2` build under
  the chosen toolchain. Consider target-gating these deps if a pure-Rust decompressor is
  acceptable on Windows.

### 4. `devices` / `vmm` / `libkrun` Windows integration  ·  effort: ? (unknown until #2 clears)
- These crates are *behind* the `imago` failure, so their Windows code errors haven't been
  surfaced yet. Expect a batch of: missing `#[cfg]` arms, Unix-only `std::os::fd` / `RawFd`
  usage, `epoll`/`eventfd` call sites needing the Windows shims, file-descriptor vs HANDLE
  mismatches.
- **TODO:** once #2 is gated, run a full `cargo check --target x86_64-pc-windows-gnu -p libkrun`
  and triage the resulting error categories. This is the first honest measure of remaining
  code work.

### 5. Build / packaging — no Windows target  ·  effort: M
- The `Makefile` has no `krun.dll` path (only `.so` / `.dylib`). No codesigning analog
  needed, but WHP requires the app to declare/Enable the Windows Hypervisor Platform.
- **TODO:**
  - Add a Windows branch to the `Makefile` (or document a `cargo build` + post-step) that
    produces `krun.dll` + import lib + `krun.h`.
  - Decide MSVC vs GNU as the supported ABI for shipped artifacts.
  - Kernel blob: confirm `libkrunfw` produces a Windows-loadable blob (today it emits
    `.so`/`.dylib`; Windows needs a `.dll` or an embedded-bytes path). **This is a parallel
    libkrunfw work item** — see "libkrunfw" below.

### 6. smolvm's own features are Unix-specific  ·  effort: L (for full parity)
- **TSI networking:** the host side of TSI uses Unix domain sockets; INET hijack works but
  the host plumbing and `KRUN_TSI_HIJACK_UNIX` path are Unix-built.
- **control-socket / fork / snapshot:** the control-socket and the fork bits are
  `#[cfg(unix)]`. Windows has no `fork`; these need a Windows design or to be disabled.
- **TODO:** decide the Windows feature baseline. Likely v1: boot a VM via WHP with virtio
  console + (gated) block, INET networking; defer fork/snapshot/control-socket and Unix-TSI.

## Phased roadmap

### Phase 0 — Make it compile (gate, don't port)
- [ ] Finish `krun-utils` edition cleanup (#1).
- [ ] Gate `imago` + block-COW off on Windows (#2 option 1).
- [ ] Get C deps building under mingw/MSVC (#3).
- [ ] Full `cargo check -p libkrun` for Windows; triage and fix the surfaced code errors (#4).
- **Exit criteria:** `cargo build --target x86_64-pc-windows-* -p libkrun` succeeds.

### Phase 1 — Produce a real artifact
- [ ] `Makefile`/build produces `krun.dll` + `krun.h` (#5).
- [ ] `libkrunfw` produces a Windows-consumable kernel blob (see below).
- [ ] Link a trivial C example against `krun.dll`.
- **Exit criteria:** a `krun.dll` + matching kernel blob exist and link.

### Phase 2 — Boot a VM
- [ ] Provision a Windows host with the Windows Hypervisor Platform feature enabled
      (Win10/11 Pro or Server; WHP not available in all SKUs / nested-virt setups).
- [ ] Bring up CI or a manual runner on that host (cross-builds can't run WHP).
- [ ] Debug first boot: partition create, memory map, vCPU run loop, serial console output.
- **Exit criteria:** a guest reaches userspace and prints to the virtio console.

### Phase 3 — Feature parity (incremental)
- [ ] Block device (revisit `imago`/COW story, #2 option 2/3).
- [ ] Networking: confirm INET TSI on Windows; design Unix-TSI or drop it.
- [ ] virtio-fs (Windows passthrough scaffolding already present — wire it up).
- [ ] Decide fate of fork/snapshot/control-socket on Windows (#6).

## libkrunfw (parallel track)

Windows needs a kernel blob it can load. Today libkrunfw emits `libkrunfw.so`/`.dylib`
wrapping a Linux kernel `Image`. Work items:
- [ ] Decide delivery: a `krunfw.dll` exporting the blob symbols, or embed the kernel bytes
      directly into `krun.dll` at build time.
- [ ] Adapt `bin2cbundle.py` output for the MSVC/GNU object format (it currently targets ELF).
- [ ] Confirm the guest kernel build itself is unaffected (it's still a Linux kernel; only
      the *wrapper* artifact format changes for the Windows host).

## Test / verification strategy
- **Cross-build gate (CI, any OS):** `cargo check --target x86_64-pc-windows-gnu` to keep
  Windows compiling — cheap, catches `#[cfg]` regressions.
- **Native build (Windows runner):** MSVC build producing `krun.dll`.
- **Runtime (Windows + WHP host):** boot smoke test, console output, then per-feature tests.
- A cross-build gate is worth adding **now** (Phase 0) so Windows doesn't silently re-break.

## Effort summary

| Phase | Effort | Risk |
|---|---|---|
| 0 Compile | M–L | imago is the wildcard |
| 1 Artifact | M | libkrunfw blob format |
| 2 Boot | M | needs real WHP host; first-boot debugging |
| 3 Parity | L | TSI/fork/snapshot are Unix-designed |

**Overall:** realistically multi-week. The hypervisor backend (the part that's usually
hardest) is already compiling; the cost is in the dependency graph (`imago`), the
build/packaging plumbing, and re-homing smolvm's Unix-specific features.

## Open questions
- Is block-COW required for the first Windows release, or can it ship raw-disk-only?
- MSVC or GNU ABI for shipped `krun.dll`?
- Which WHP-capable host(s) do we have for CI / manual testing?
- Minimum viable Windows feature set — is "boot + console + INET net" acceptable for v1?
