# FDS Phase 3 — IPS sidecar disk-save persistence

Handoff note for a fresh session. Phase 1 (disk transport + BIOS
memory map) and Phase 2 (F4 disk-swap UI) are committed and working.
Phase 3 adds save persistence so FDS games retain progress across
runs.

## Current state (what already works)

- Mapper 20 (`src/mapper/fds.rs`) fully functional: BIOS at
  `$E000-$FFFF`, 32 KiB PRG-RAM at `$6000-$DFFF`, 8 KiB CHR-RAM,
  disk transport state machine at `$4020-$4033`, 16-bit timer IRQ +
  disk-transfer IRQ.
- `fdsirqtests.fds` passes 20/20.
- Zelda no Densetsu boots, title screen, disk swap via F4 →
  advances past the "please insert side B" prompt to gameplay.
- `FdsControl` trait in `src/mapper/mod.rs` — disk eject/insert
  API with post-swap pause (`SWAP_EJECT_CYCLES = 500_000`).
- 330 unit tests pass, full hardware regression green.
- BIOS resolver searches `$XDG_CONFIG_HOME/vibenes/bios/disksys.rom`
  (plus env / CLI / rom-adjacent tiers). Known-good CRC32 is
  `0x5E607DCF`.

## What's broken / missing

- **Saving doesn't persist.** You can save in Zelda, quit, reopen,
  and the save slot is gone. The mapper modifies `gapped_sides` in
  RAM via `WriteFdsDisk`, but nothing flushes to disk.

## Phase 3 goal

Persist FDS save data as `.ips` sidecars next to the ROM's iNES
`.sav` siblings. Path scheme should reuse `src/save.rs`'s
`save_path_for` pipeline with an `.ips` extension override. Hook
into the existing save triggers (quit / ROM swap / periodic
autosave at 3-minute intervals) that already work for iNES
battery carts.

## Design decision: gapped-IPS vs raw-IPS

FDS disk data lives in two forms internally:
- **Raw**: the on-disk `.fds` file format, 65500 bytes/side, bare
  block data with no gaps or sync markers.
- **Gapped**: the scan-ready form the transport actually reads —
  28300-bit leading gap + 0x80 sync before each block + 2 fake CRC
  bytes + 976-bit inter-block gap. Built by `FdsImage::gapped_sides`
  at load. Games write into this buffer during play.

**User preference:** aim for **Option B (Mesen2-compatible raw-IPS)**
but Option A (simpler gapped-IPS) is acceptable if the port of
`RebuildFdsFile` becomes painful.

### Option A — gapped-IPS (~100 LOC)

- On save: clone a reference copy of the original gapped sides at
  load time; diff current vs reference using the `src/fds/ips.rs`
  encoder already written for Phase 0; write as `.ips`.
- On load: apply IPS to the original gapped sides, use as runtime
  buffer.
- Pros: no new code beyond dirty tracking + `disk_save_data` plumbing.
- Cons: `.ips` files are vibenes-only. Mesen2 won't read them (it
  expects raw-coords offsets).

### Option B — raw-IPS (~200 LOC, cross-emulator interop)

- Port Mesen2's `FdsLoader::RebuildFdsFile`
  (`~/Git/Mesen2/Core/NES/Loaders/FdsLoader.cpp:93-142`) — walks
  gapped bytes, emits only block data bytes to reconstruct the
  65500-byte-per-side raw format.
- On save: rebuild raw from current gapped sides, diff against
  original raw, IPS-encode.
- On load: apply IPS to original raw, then run `add_gaps()` to
  produce the runtime gapped sides.
- Pros: `.ips` files round-trip with Mesen2 — users can move saves
  between emulators. Future-proofs if vibenes savestates ever
  want to read/write the same format.
- Cons: more code, and `RebuildFdsFile` has a per-block state
  machine (track block type → length, file-data block length
  derived from previous file-header's size field — same walker
  pattern as `add_gaps` but inverse).

**Recommendation: start with Option B.** `RebuildFdsFile` is ~50 LOC
of straightforward state-machine code; the port + tests are a
half-day. Cross-emulator save interop is a small but real win, and
matches Mesen2's saved behavior so our own round-trip tests can
use Mesen2's IPS files as oracles.

## File-level action items

### 1. Extend `FdsData` in `src/fds/mod.rs`

Add fields to preserve the original state for diffing at save time:

```rust
pub struct FdsData {
    pub gapped_sides: Vec<Vec<u8>>,
    pub headers: Vec<Vec<u8>>,
    pub bios: Vec<u8>,
    pub bios_known_good: bool,
    pub had_header: bool,

    // Phase 3 additions:
    /// Per-side raw bytes at load time. Used as the IPS diff base
    /// on save. Clone of `FdsImage::sides` post-fwNES-header-strip.
    pub original_raw_sides: Vec<Vec<u8>>,
}
```

(No need to keep original gapped sides if we reconstruct raw on
save — we always reconstruct from the current gapped buffer and
diff against `original_raw_sides`.)

### 2. Port `RebuildFdsFile` in `src/fds/image.rs` (Option B only)

New pub fn — walks a gapped side, emits the 65500-byte raw side:

```rust
/// Inverse of `add_gaps`. Given a gapped side (the kind the FDS
/// transport scans over), reconstruct the 65500-byte raw
/// representation by emitting only block data bytes (dropping
/// gaps, sync markers, and the 2 fake CRC bytes after each block).
/// Matches Mesen2's `FdsLoader::RebuildFdsFile`.
pub fn rebuild_raw(gapped_side: &[u8]) -> Vec<u8> { ... }
```

Block-walker state machine:
- Skip bytes until `0x80` sync.
- Block type is the next byte.
- Length by type: 1 → 56, 2 → 2, 3 → 16, 4 → `1 + file_size`.
  `file_size` comes from the preceding block-3's bytes 13-14.
- After block data: skip 2 bytes (fake CRC), skip gap bytes until
  next `0x80` or end-of-side.
- Pad the output to exactly 65500 bytes with zeros.

Unit test: round-trip `raw → add_gaps → rebuild_raw` must equal
the original raw. Seed with the synthesized blocks already in
`src/fds/image.rs::tests::gapped_sides_structures_blocks_with_syncs_and_crcs`.

### 3. Dirty tracking in `src/mapper/fds.rs`

- Add `save_dirty: bool` field, initialized `false`.
- Flip to `true` in `write_disk_byte` when the byte actually
  changes (existing write_disk_byte already compares — just add
  the flag flip).
- `save_dirty` surfaces through the new `Mapper::disk_save_dirty`
  trait method.

### 4. New `Mapper` trait methods in `src/mapper/mod.rs`

Parallel to the existing battery methods (`save_data` /
`load_save_data` / `save_dirty` / `mark_saved`) but distinct so
FDS can carry both a null battery hook (no `$6000-$7FFF` iNES
PRG-RAM) and a real disk save:

```rust
/// FDS disk save data as an IPS patch. `None` on non-disk carts.
fn disk_save_data(&self) -> Option<Vec<u8>> { None }

/// Apply an IPS patch to restore disk state. Called once at ROM
/// load, before the CPU reset sequence.
fn load_disk_save(&mut self, _ips_bytes: &[u8]) {}

/// True when any disk byte has been modified since the last
/// `mark_disk_saved` call.
fn disk_save_dirty(&self) -> bool { false }

fn mark_disk_saved(&mut self) {}
```

All default to no-op; only `mapper/fds.rs` overrides.

FDS impl of `disk_save_data`:
```rust
fn disk_save_data(&self) -> Option<Vec<u8>> {
    // Concatenate current raw sides, one after another (matches
    // Mesen2's RebuildFdsFile output layout when needHeader=false).
    let current_raw: Vec<u8> = self
        .disk_sides  // gapped
        .iter()
        .flat_map(|side| rebuild_raw(side))
        .collect();
    let original_raw: Vec<u8> = self
        .original_raw_sides
        .iter()
        .flat_map(|s| s.iter().copied())
        .collect();
    Some(ips::encode(&original_raw, &current_raw).ok()?)
}
```

### 5. Wire through `Nes` in `src/nes.rs`

Mirror `save_battery` / `load_battery`:

```rust
pub fn save_disk(&mut self, cfg: &SaveConfig) -> Result<bool> { ... }
pub fn load_disk(&mut self, cfg: &SaveConfig) -> Result<bool> { ... }
```

Use the existing `save_path_for_with_ext(cfg, self.save_meta, "ips")`
helper (will need to be added to `src/save.rs` if not already —
`save.rs` currently uses `SAVE_EXT = "sav"` directly; factor the
extension out into a parameter). Atomic write through the same
tempfile+rename pipeline.

### 6. Hook into flush triggers in `src/main.rs`

Every site that currently calls `save_battery` should also call
`save_disk`. Searchable via `save_battery` occurrences:
- `shutdown()` — quit / Esc / window close
- `load_rom()` — ROM swap flushes outgoing cart
- Autosave tick at 3-minute interval (existing
  `autosave_every_n_frames` in `SaveConfig`)

Symmetrically: `load_rom()` should call `nes.load_disk(cfg)`
right after `attach_save_metadata`, parallel to `load_battery`.

## Test plan

### Unit tests (in `src/fds/image.rs` + `src/mapper/fds.rs`)

- `rebuild_raw` round-trip: `raw → add_gaps → rebuild_raw == raw`.
- FDS mapper writes to disk → `disk_save_dirty` becomes true.
- `mark_disk_saved` clears dirty.
- `disk_save_data` after a single-byte write produces a small IPS
  patch; `load_disk_save` applied to a fresh cart reproduces the
  byte.
- Empty-diff (no writes) produces `PATCH` + `EOF` only (8 bytes).

### Manual gate

- Launch Zelda no Densetsu, swap to side B, save a character name.
- Quit.
- Relaunch → character select should show the saved name.
- Inspect `~/.config/vibenes/saves/Zelda*.ips` — should exist and
  be small (~100-500 bytes).

### Interop (Option B only)

- After vibenes saves a Zelda game, load Mesen2 on the same ROM
  with that `.ips` file copied to its save dir. Character should
  appear in Mesen2's slot too.

## References

- Mesen2 `Core/NES/Mappers/FDS/Fds.cpp:87-103` — `SaveBattery`
  showing the raw-rebuild + IPS-diff flow.
- Mesen2 `Core/NES/Loaders/FdsLoader.cpp:93-142` — `RebuildFdsFile`
  block walker (this is the one to port).
- `src/fds/ips.rs` already has `encode` / `apply` — no work needed
  on the codec itself.
- `src/fds/image.rs::add_gaps` is the inverse walker; mirror its
  block-type dispatch in `rebuild_raw`.
- Existing battery-save plumbing in `src/save.rs` + `src/nes.rs` +
  `src/main.rs` — parallel the pattern exactly, just different
  extension.

## Gotchas to watch for

- **fwNES header byte on save**: if the original `.fds` had the 16-
  byte fwNES header, Mesen2 re-emits it when rebuilding. For
  Phase 3 it's enough to save a headerless IPS of just the sides;
  the `had_header` flag can drive whether to include the 16-byte
  prefix in `original_raw` when diffing. Simpler: skip the header
  on both sides of the diff (save + load), since the header is
  constant.
- **Dirty flag false-positives**: the disk-transport's
  `WriteFdsDisk` already compares byte-before-write. Keep that
  behavior — without it, every motor-on scan through idle bytes
  would dirty the save.
- **`save_dirty` must be set ONLY on real data modifications**,
  not on gap-byte or CRC-byte writes during the transport cycle.
  Those go through `WriteFdsDisk` too. Conservative: every
  `WriteFdsDisk` dirties. Mesen2 does this and it's fine because
  the IPS diff is empty for unchanged bytes anyway — just slightly
  more CPU on the diff pass.
- **Autosave interval**: existing `autosave_every_n_frames =
  60 * 60 * 3` is 3 minutes. Fine for disk saves too — no need
  to special-case.
- **Don't lose the original**: the `.ips` file must be written
  atomically. `src/save.rs`'s existing `write_atomically` handles
  this; don't reinvent.

## Scope excluded from Phase 3

- FDS audio synthesis (phase 4+, expansion-audio work).
- Full snapshot states (separate future project).
- Mesen2's `$E445` auto-insert header-matching hack — the F4 UI
  from Phase 2 handles multi-side prompts fine.
- `$4032` 20-poll fallback eject heuristic — same reason.
- `.qd` (Quick Disk) format support — no test ROMs in the library.
