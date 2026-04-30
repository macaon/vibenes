# Plan: Famicom microphone input

**Status:** Plan only - not implemented.
**Date drafted:** 2026-04-30
**Triggers:** mapper 188 (Bandai Karaoke Studio) bring-up; FDS *Zelda no Densetsu* / *Kid Icarus* / *Raid on Bungeling Bay*.

## Why this is needed

Two distinct mic paths exist on real Famicom hardware. vibenes currently models neither.

1. **Famicom built-in mic** (player 2 controller jack). Comparator output readable as
   bit 2 of `$4016`. NES P2 jack omits this pin, so NES-region carts must read 0.
   - *Zelda no Densetsu* (FDS) - damages Pol's Voice / Pols Voice
   - *Kid Icarus / Hikari Shinwa: Palutena no Kagami* (FDS) - same enemy class
   - *Raid on Bungeling Bay* - blowing into the mic spawns the foghorn boss
   - A handful of other FDS / Famicom carts
2. **Bandai Karaoke Studio** (mapper 188). Cart's mic comparator drives bits of
   `$6000-$7FFF` reads (D0/D1 per Mesen2; verify exact mask before implementing).
   One licensed game + 3 song add-on carts.

Without this, FDS Zelda fights are unwinnable past Pol's Voice and the Karaoke
Studio mapper has no input surface.

## Current vibenes state

- `src/nes/bus.rs:255-259` only reads bit 0 from `controllers[0/1]`. No bit-2 path.
- No mic state anywhere in the tree.
- cpal is already pulled in for audio output - input streams are free dependency-wise.
- mapper 188 work paused so the cross-cutting input layer can be designed first.

## Architectural decisions (load-bearing)

| | Decision | Rationale |
|-|-|-|
| **D1** | **Signal model: 1-bit binary `bool`.** | Real comparator only emits 0/1; games count edges, not amplitude. Easy to test deterministically. Continuous level buys nothing because no cart ever ADCs the mic. |
| **D2** | **State lives as `Bus::mic_active: bool`** (not a separate input module). | Bus already owns the input plane (`controllers: [Controller; 2]`); mic is one more bit on the same plane. No `Rc<Cell<u8>>`, no shared interior mutability. |
| **D3** | **New trait method `Mapper::cpu_read_with_mic(&mut self, addr, mic) -> u8`** with default `self.cpu_read(addr)`. Bus call-site in `0x6000..=0xFFFF` arm passes `self.mic_active`. | Localized; only mapper 188 overrides. `cpu_read_ex` is reserved for `$4020-$5FFF` cart-claimable expansion space - don't muddy that contract. Generalizes cleanly to `cpu_read_with_input(&InputState)` when Family BASIC keyboard lands. |
| **D4** | **Capture `system_is_famicom: bool` at `Bus::new`** from `Cartridge::system` (resolved by gamedb). `$4016` bit-2 read ANDs the mic bit with this flag. | Zero-cost gate on the hot read path. Matches how `mapper_id` is captured. |
| **D5** | **Mic state NOT serialized** in `BusSnap`. Comment on the field to make this explicit. | Mic is transient host input - the host pushes the next state on the next frame after a save-state load. |
| **D6** | **Source-agnostic publishers** push to `Bus::mic_active`: keyboard, cpal, silent, test setter. The bit reaching the bus is the same regardless. | Threshold/hysteresis lives in the cpal adapter, NOT in the bus. Keeps the core dumb. |
| **D7** | **Concurrency: `AtomicBool` if any non-emulator thread publishes.** | cpal callback runs on its own thread. Use `Relaxed` ordering - real hardware reads whatever the comparator latched at the instant of the read; race semantics are acceptable. |

## Phases

### Phase 1 - Bus state (~15 lines)

**Files:** `src/nes/bus.rs`

- Add `mic_active: AtomicBool` (or plain `bool` if we ship without cpal first;
  upgrade to `AtomicBool` in Phase 5b).
- Add `system_is_famicom: bool`. Read from `Cartridge::system` in `Bus::new`.
  Decide whether to thread `rom::System` into `Bus::new` directly (preferred,
  ~10 callers) or via a setter.
- Add `pub fn set_mic_active(&self, active: bool)` (`&self`, not `&mut self`,
  if AtomicBool) and `pub fn mic_active(&self) -> bool`.
- `Bus::reset` does NOT touch `mic_active` (host owns this state, reset doesn't
  release the user's finger / mic).

### Phase 2 - $4016 bit 2 wiring (~5 lines)

**Files:** `src/nes/bus.rs` (`0x4016` arm in `read`)

```rust
// Famicom built-in mic, P2 controller-jack pin 13. Gated on system tag
// because NES P2 jack omits the mic line.
let mic = (self.mic_active() && self.system_is_famicom) as u8;
0x4016 => (self.open_bus & 0xA0) | 0x40 | (self.controllers[0].read() & 1) | (mic << 2),
```

`$4017` left untouched (mic is on $4016 path, not $4017 - verified against
Mesen2 and nesdev wiki).

### Phase 3 - Mapper trait extension (~15 lines)

**Files:** `src/nes/mapper/mod.rs`

```rust
/// PRG-space read with current mic state. Default forwards to `cpu_read`.
/// Mapper 188 (Bandai Karaoke Studio) is the only override at first.
/// Future: rename to `cpu_read_with_input(&InputState)` when Family BASIC
/// keyboard lands - the trait shape generalizes.
fn cpu_read_with_mic(&mut self, addr: u16, _mic: bool) -> u8 {
    self.cpu_read(addr)
}

/// Side-effect-free counterpart for debuggers / save-state diff tools.
fn cpu_peek_with_mic(&self, addr: u16, _mic: bool) -> u8 {
    self.cpu_peek(addr)
}
```

Bus call-site in `0x6000..=0xFFFF` read arm switches to `cpu_read_with_mic`.
Verify no other code path bypasses `Bus::read` for $6000-$FFFF (check
`dummy_read`, `cpu_peek` callers).

### Phase 4 - Mapper 188 mic integration (~10-20 lines)

**Files:** `src/nes/mapper/bandai_karaoke.rs` (in flight as of 2026-04-30)

- Override `cpu_read_with_mic`. When `addr` ∈ `0x6000..=0x7FFF`, OR the mic-bit
  mask into the returned byte if `mic` is true.
- **Bit-mask correctness is load-bearing.** Cross-check
  `~/Git/Mesen2/Core/NES/Input/BandaiMicrophone.h` AND
  `~/Git/punes/src/core/` AND a *Karaoke Studio* disassembly snippet before
  committing. Lock the constant in a `pub const` with citing comment.
- Keep `cpu_read` returning the no-mic byte so debugger peek stays clean.

### Phase 5 - Host keyboard binding (~10 lines)

**Files:** TBD - search `src/bin/`, `src/app/`, or wherever winit events pump

- Bind `'I'` (Mesen's default mic key) to `bus.set_mic_active(pressed)` on
  `KeyDown` / `KeyUp`.
- If host input pump can't be located in this pass, leave a single-line TODO
  near `Bus::set_mic_active` and ship Phases 1-4 + 6 anyway. Core is testable
  without it.

### Phase 5b - cpal mic adapter (~80 lines, separate PR)

**Files:** new `src/audio/mic_input.rs` (or under existing audio module),
small wiring in `Bus::new` callers.

```rust
pub struct MicInput {
    _stream: cpal::Stream,
    active: Arc<AtomicBool>,
}

impl MicInput {
    pub fn start(
        threshold_dbfs: f32,    // default -30.0
        hysteresis_dbfs: f32,   // default  -6.0 (release at threshold + this)
    ) -> anyhow::Result<Option<Self>> { ... }

    pub fn active(&self) -> bool { self.active.load(Relaxed) }
}
```

- Pick `cpal::Host::default_input_device()`. Return `Ok(None)` if no device -
  graceful degradation, log a warning, fall through to keyboard-only.
- Stream callback computes RMS over each block (~10 ms / 480 samples @ 48 kHz).
- Hysteresis: rising threshold = `threshold_dbfs`, falling threshold =
  `threshold_dbfs - hysteresis_dbfs`. Prevents chatter on borderline signals.
- The emulator thread polls `mic_input.active()` once per frame (60 Hz is
  plenty - games sample $4016 bit 2 at ≤60 Hz for game-correctness purposes)
  and pushes via `bus.set_mic_active`.
- Sample-format negotiation: cpal's `SupportedStreamConfigRange` exposes both
  `f32` and `i16`. Implement both callback variants, pick at stream-config
  time.
- CLI / config: `--mic=cpal|keyboard|off`, default `keyboard`. Users without
  a mic stay on the keyboard path.

### Phase 6 - Tests across all of the above (~100 lines)

**Files:** `src/nes/bus.rs` (`#[cfg(test)] mod mic_tests`),
`src/nes/mapper/bandai_karaoke.rs` (mic-specific test module).

Synthetic unit tests (no commercial mic test ROM exists):

1. Famicom mic ON, read `$4016`, assert `(value & 0x04) == 0x04`.
2. Famicom mic OFF, read `$4016`, assert bit 2 clear.
3. NES-mode mic ON, read `$4016`, assert bit 2 still clear (gate works).
4. Mic state does NOT affect bit 0 of `$4016` (regression guard).
5. Mic state does NOT affect any bit of `$4017` in either system.
6. Mapper 188 + mic ON, read `$6000`, assert mic-bit mask set.
7. Mapper 188 + mic OFF, read `$6000`, assert mic bits zero (rest of byte
   matches `cpu_read` directly).
8. `cpu_peek_with_mic` symmetry: same scenarios as 6/7 via the peek path,
   no state mutation, byte-for-byte match.
9. Save-state round trip drops mic: set `mic_active = true`, capture, restore
   on a fresh `Bus` with `mic_active = false`, assert the restored bus has
   `mic_active == false`.
10. Reset preserves `mic_active`: with mic held, call `Bus::reset`; assert
    the bit is still set.

## Dependencies

- Phase 1 → Phase 2 (mic_active + system_is_famicom must exist).
- Phase 1 → Phase 3 (bus call-site reads `mic_active`).
- Phase 3 → Phase 4 (mapper 188 implements the trait method).
- Phases 1-4 → Phase 5 (host binding only matters once core works).
- Phases 1-4 → Phase 5b (independent of Phase 5; both publish to the same bit).
- Phases 1-5 → Phase 6 (tests exercise everything).

## Risks

- **Wrong bit mask on mapper 188.** Karaoke Studio polls a specific mask;
  wrong shift = no mic detection. *Mitigation:* cross-check Mesen2 + puNES +
  game disassembly before merging Phase 4.
- **System tag missing for Famicom-only carts not in gamedb.** Mic gate stays
  off, games softlock waiting for input. *Mitigation:* document; add
  `--force-system=famicom` host flag (defer if no CLI surface). FDS ROMs
  auto-Famicom via `Cartridge::is_fds`, covering Zelda + Kid Icarus.
- **Family BASIC keyboard collision.** HVC-007 multiplexes scan codes onto
  $4016/$4017. *Mitigation:* trait shape (`cpu_read_with_mic` →
  `cpu_read_with_input(&InputState)`) generalizes cleanly. Don't ship keyboard
  in this plan.
- **Save state accidentally captures mic.** Future "for completeness" addition
  could break the host-owned model. *Mitigation:* explicit `// NOT in BusSnap`
  comment on the field.
- **Threading `System` into `Bus::new` breaks test builders.** *Mitigation:*
  count callers at Phase 1; if <10 do direct ctor update, else add
  `Bus::with_system` helper.
- **cpal-specific:**
  - Cross-platform device pickers (PipeWire/macOS/WASAPI quirks). Fall back to
    keyboard on error.
  - Sample-format negotiation. Implement both `f32` and `i16` callbacks.
  - Mic permissions (macOS Info.plist, Linux portals). Document; surface clear
    error if device opens but returns no samples.
  - Threshold tuning. Default conservative (-30 dBFS / -36 dBFS hysteresis);
    expose via CLI / settings.

## Out of scope (explicit)

- Family BASIC HVC-007 keyboard. Trait shape leaves the door open; no code now.
- Continuous analog mic level. Binary is the production model.
- Pulse-shape "blowing" feel. Host-side enhancement, defer.
- Mic in save-state snapshots. Excluded by D5.
- In-app mic sensitivity slider. Ship with CLI flag + sensible defaults.

## Recommended ship order

Two PRs, both small and independent:

1. **PR 1: Phases 1-4 + Phase 5 + Phase 6.** ~150-160 lines.
   Lands a working keyboard-driven mic for Zelda / Kid Icarus / Karaoke Studio.
2. **PR 2: Phase 5b.** ~80 lines.
   Bolts on cpal as an alternative source. PR 1 is correct and complete on
   its own; users who want real mic opt in via flag.

## Files touched

| Path | Phases |
|------|--------|
| `src/nes/bus.rs` | 1, 2, 6 |
| `src/nes/mapper/mod.rs` | 3 |
| `src/nes/mapper/bandai_karaoke.rs` | 4, 6 |
| `src/nes/rom.rs` | (read-only consult for `System` enum) |
| `src/nes/mod.rs` | 1 (optional `Nes::set_mic_active` pass-through) |
| host input pump (`src/bin/run.rs` or similar) | 5 |
| `src/audio/mic_input.rs` (new) | 5b |

Reference reading (clean-room comprehension only):
- `~/Git/Mesen2/Core/NES/Input/BandaiMicrophone.h`
- `~/Git/Mesen2/Core/NES/Input/FamicomController.h` (if present)
- `~/Git/punes/src/core/` for comparator behavior cross-check
- nesdev wiki: `Famicom_Microphone`, `INES_Mapper_188`

## Success criteria

- [ ] `Bus::set_mic_active(true)` + `$4016` read on Famicom cart returns bit 2 set.
- [ ] Same on NES cart returns bit 2 clear.
- [ ] `$4017` unaffected by mic state in both modes.
- [ ] Mapper 188 `cpu_read_with_mic($6000, true)` returns mic-bit mask set;
      `false` returns no mic bits.
- [ ] `cpu_peek_with_mic` byte-for-byte matches `cpu_read_with_mic` (no side
      effects).
- [ ] Save-state round trip does NOT carry mic state.
- [ ] All existing `cargo test --lib --release` pass; full regression suite
      (`instr_test-v5` 16/16, `apu_test` 8/8, `apu_reset` 6/6, etc.) green.
- [ ] Pressing `'I'` in host frontend toggles mic input through to bus.
- [ ] Manual smoke: Zelda no Densetsu kills a Pol's Voice when `'I'` held;
      Karaoke Studio responds to mic on title screen.
- [ ] (Phase 5b) cpal source: blowing into the default input device damages
      a Pol's Voice without keyboard interaction; threshold survives quiet
      and noisy rooms with sensible defaults.
