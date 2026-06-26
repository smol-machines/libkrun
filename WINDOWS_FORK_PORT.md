# Windows fork / snapshot port — scoping & plan

Status: **scoped, not started** (2026-06-26)

Goal: bring libkrun's checkpoint / fork (1→N fast clones) to Windows/WHP so it
has parity with `linux-x86_64` and `macos-aarch64` (the only platforms it is
gated to today).

## How fork works on the supported platforms

1. A "golden" VM is paused and **frozen** (`checkpoint_for_fork`): vCPU + VM +
   device state captured, and guest RAM left mapped from a backing object.
2. Guest RAM is **CoW-shared cross-process**: Linux uses `memfd` reached by
   clones via `/proc/<pid>/fd`; macOS uses a backing **file** reached by path.
   Each clone `mmap(MAP_PRIVATE)`s it → shares clean pages, copies on write.
3. A clone process restores the checkpoint (registers/devices) and maps the
   golden RAM CoW, so it boots instantly into the golden's state.

## What is missing on Windows (all of it)

- **vCPU state is an empty stub.** `windows/vstate.rs::VcpuState` serializes to
  nothing. Needs capture/restore of the full architectural state via
  `WHvGet/SetVirtualProcessorRegisters`: GPRs, RIP/RSP/RFLAGS, segment +
  table registers (union values), CR0/2/3/4/8, XCR0, EFER, the MSRs, **FPU/XMM
  (XSAVE)**, and the **LAPIC / interrupt-controller** state.
- **No XSAVE / interrupt-controller bindings.** The `whp` crate wraps GP-register
  get/set only; `WHvGet/SetVirtualProcessorXsaveState` and
  `…InterruptControllerState[2]` would have to be added.
- **CoW guest RAM is Linux/macOS-only.** Windows guest RAM isn't section-backed;
  fork needs a pagefile-backed section (`CreateFileMapping`) or backing file +
  `MapViewOfFile(FILE_MAP_COPY)`, **and WHP must `WHvMapGpaRange` that CoW view**
  with correct copy-on-write semantics.
- **Cross-process sharing.** No `/proc/<pid>/fd`; clones would reach the golden's
  section by name or a duplicated handle (closest to the macOS file-path model).
- **Device snapshot/restore are no-ops.** e.g. `windows/passthrough.rs`
  `snapshot()/restore()` are explicit TODO stubs; each Windows device needs real
  logical-state capture/restore.
- **Restore path + C API gated off Windows** (`restore_ctx = None`,
  `checkpoint`/`checkpoint_for_fork`/`build_restore_ctx` cfg-gated).

## THE key risk

WHP is a higher-level hypervisor than KVM and exposes **less** in-partition
state. The codebase already notes "WHP has no host-serializable in-partition
device state … no in-kernel IRQ chip/PIT analogue exposed." The open question is
whether `WHvGet/SetVirtualProcessorInterruptControllerState` (+ XSAVE) is enough
to **round-trip a vCPU cleanly**, and whether WHP will map a `PAGE_WRITECOPY`
view as guest RAM. If not, fork is not achievable on WHP regardless of effort.

## Recommended plan (de-risk first)

1. **Feasibility spike (do this before committing):** implement WHP vCPU
   register save/restore (GPRs + RIP/RSP/RFLAGS + CR + EFER + segments + XSAVE +
   interrupt-controller), and a tiny in-process test: pause → save → mutate a
   register → restore → verify. Answers "can WHP round-trip vCPU state?" This is
   the foundation everything else needs and is independently valuable.
2. **In-process eager checkpoint/restore** (rewind): ungate `checkpoint`/
   `restore`, add Windows device snapshot/restore, copy guest RAM eagerly. No
   CoW/cross-process yet — proves a full state round-trip works.
3. **Windows CoW guest RAM**: section/file-backed forkable memory +
   `MapViewOfFile(FILE_MAP_COPY)` + `WHvMapGpaRange`.
4. **Cross-process fork**: section sharing golden→clone; ungate
   `checkpoint_for_fork` / `build_restore_ctx`; restore path.
5. **Validate** a real fork on the WHP thinkpad.

Phases 1–2 are the high-value, feasibility-gating work; 3–5 are the heavy,
WHP-dependent lift. Estimated multi-day-to-week with a real chance phase 1
reveals a WHP limitation.
