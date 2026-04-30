# vibenes

A cycle-accurate NES emulator in Rust.

## About this project

The goal was simple: find out whether a maintainable, cycle-accurate
NES emulator could be built with AI as the coding assistant, without
directly porting code from existing emulators. Every subsystem had to
land with a passing suite of hardware test ROMs before the next one
started.

Initially the assistant was allowed to work only with a skill trained
on hardware specs and material available on https://www.nesdev.org.
Getting a working emulator going was done quickly, but the assistant
had no grasp of how accuracy should be achieved, and while it ran roms
mostly fine, There were noticable timing issues (with the APU in
particular).
With a tighter direction on the 3rd rewrite from scratch, the master
clock variant came to be and the emulator was mostly accurate, and
started passing Blargg tests. NMI was a bigger challenge though,
and I started allowing the assistant access to the source code from
Mesen2, puNES and Nestopia to use as reference material, with a "no
copy or port" rule. The material was used by the agent to understand
the math.

The strict "no copying" rule held for the CPU, PPU, APU, bus, DMA, and
the first batch of mappers. Later, some well-trodden corners (FDS
audio synth, VRC6 audio, FDS disk-image rebuild) were ported directly
from Mesen2 under GPL-3.0-or-later compatibility rather than
reinventing well-understood math. Those ports are attributed in the
commit history and in the source files.

This is an AI-assisted re-implementation that used Mesen2, puNES, and
Nestopia as behavioral references, with a handful of direct ports
where GPL compatibility allowed it and reinvention offered no benefit.

### So what's next?

I will continue developing this emulator using AI as a personal project.
Usability and accuracy will continue to be the most important goals,
and if you'd like to test a Rust based NES emulator, feel free to
clone this and give it a whirl.

To be clear, vibenes would NOT be accurate to the level it is today
without the hard work that went into Mesen2, Nestopia and puNES,
along with all the research material from NesDev. This AI assisted
project does not aim to or claim to be surpassing these products or
developers in any way.

## Status

### Subsystems

- iNES 1.0 / NES 2.0 loader with CRC32-keyed game DB
- 6502 CPU core, all 256 opcodes, cycle-accurate interrupts
- Master clock + bus with unified parity-gated DMC + OAM DMA loop
- PPU with full render pipeline, pixel-precise sprite-0 hit,
  odd-frame dot skip, open-bus decay
- APU with 5 channels (pulse 1+2, triangle, noise, DMC), frame
  counter, IRQ, DMC DMA
- Expansion audio via bus-level mixer with per-chip pre-scaled blend
- Host audio via cpal + blip_buf
- Windowed runtime on wgpu, NTSC/PAL-paced, keyboard input
- Overlay UI (egui, F1)
- Battery-backed saves with atomic writes and flush-on-quit/swap
- Save states: 10 slots, F2/F3 hotkeys, in-memory backup-on-load
  rollback, region/CRC-tagged file paths so renames and patches
  don't collide
- Famicom Disk System (mapper 20) with BIOS, disk swap, IPS sidecar
  saves, RP2C33 audio

### Supported mappers

I've opted to supporting licensed mappers only. Running multi-carts with custom
mappers and unofficial mappers overall is not in scope for now.

| # | Name | Status |
|---|---|---|
| 0 | NROM | done |
| 1 | MMC1 / SxROM | done |
| 2 | UxROM | done |
| 3 | CNROM | done |
| 4 | MMC3 / MMC6 (TxROM / HKROM) | done (Rev A + Rev B) |
| 5 | MMC5 / ExROM | done |
| 7 | AxROM | done |
| 9 | MMC2 / PxROM | done |
| 10 | MMC4 / FxROM | done |
| 16 | Bandai FCG-1/2 / LZ93D50 | done |
| 18 | Jaleco SS88006 | done |
| 19 / 210 | Namco 163 / 175 / 340 (incl. N163 8-channel wavetable audio) | done |
| 20 | Famicom Disk System | done |
| 21 / 22 / 23 / 25 | Konami VRC2a / VRC2b / VRC2c / VRC4a-f | done |
| 24 | Konami VRC6a | done |
| 26 | Konami VRC6b | done |
| 32 | Irem G-101 (incl. Major League submapper) | done |
| 33 | Taito TC0190 / TC0350 (no-IRQ subset) | done |
| 34 | BNROM (Deadly Towers; bus-conflict 32 KiB PRG bank) + NINA-001 (Impossible Mission II, Wayne's World; $7FFD-$7FFF register set with 32 KiB PRG + two 4 KiB CHR banks); auto-detect by CHR-ROM presence | done |
| 37 | Nintendo SMB + Tetris + World Cup multicart | done |
| 48 | Taito TC0690 (MMC3-style + delayed IRQ; Don Doko Don 2, Power Blazer, Flintstones, Captain Saver, The Jetsons) | done |
| 64 | Tengen RAMBO-1 (Klax JP, Skull & Crossbones, Rolling Thunder, Hard Drivin') | done |
| 65 | Irem H3001 (Spartan X 2, Daiku no Gen-san 2, Kaiketsu Yanchamaru 3; CPU-cycle 16-bit IRQ down-counter) | done |
| 66 | GxROM / MHROM | done |
| 67 | Sunsoft-3 (Fantasy Zone II, Mito Koumon) | done |
| 70 | Bandai 74*161/161/32 (Family Trainer 1-7, Famicom Jump, Hanamaru, Kamen Rider Club; bus-conflict ANDs CPU value with ROM byte) | done |
| 71 | Codemasters / Camerica BF909x (Micro Machines, Bee 52, Big Nose, Quattro singles; sub 1 / Fire Hawk auto-promotes to BF9097 with 1-screen mirror control) | done |
| 72 | Jaleco JF-17 (Pinball Quest, Wing of Madoola, Moero!! Juudou Warriors; rising-edge PRG/CHR gates + bus conflict; uPD7756C ADPCM not modeled) | done |
| 68 | Sunsoft-4 (After Burner II, Sugoro Quest; incl. CHR-as-nametable replacement and submapper-1 Maeda licensing chip) | done |
| 69 | Sunsoft FME-7 / 5A / 5B (incl. full 5B audio: tone + noise + envelope) | done |
| 73 | Konami VRC3 (Salamander) | done |
| 75 | Konami VRC1 (Tetsuwan Atom, Ganbare Goemon!) | done |
| 78 | Irem 74*161 / Jaleco JF-16 (Holy Diver sub 3 - H/V mirror, Cosmo Carrier sub 1 - single-screen; bus-conflict ANDs CPU value with ROM byte) | done |
| 80 | Taito X1-005 (Wagyan Land 2 + 3, Famista 89-Pro series, Bakushou!! Jinsei Gekijou series, Don Doko Don, Daikoukai Jidai) | done |
| 82 | Taito X1-017 (SD Keiji: Blader, Kyonshiizu 2; 5 KiB battery WRAM with three independently-gated regions) | done |
| 85 | Konami VRC7 (incl. OPLL FM audio via vendored emu2413) | done |
| 88 | Namcot Type C (extra-CHR-line wiring; Devil Man, Mendel Palace) | done |
| 89 | Sunsoft-2 with single-screen mirror (Tenka no Goikenban: Mito Koumon; bus-conflict ANDs CPU value with ROM byte) | done |
| 92 | Jaleco JF-19 (Moero!! Pro Yakyuu '88, Moero!! Pro Tennis; same chip as 72 wired with high-slot switch) | done |
| 97 | Irem TAM-S1 (Kaiketsu Yanchamaru; inverted PRG with last bank fixed at $8000, switchable at $C000; 2-mode mirroring sub 0, 4-mode sub 1) | done |
| 95 | Namco 118 / Dragon Buster (per-CHR-slot single-screen mirroring) | done |
| 118 | Nintendo TxSROM / TLSROM / TKSROM (MMC3 + per-CHR-bank dynamic mirroring; Armadillo, Goal! Two, Ys III) | done |
| 119 | Nintendo TQROM (MMC3 + 8 KiB CHR-RAM/ROM mix per slot; High Speed, Pin*Bot, Mall Madness) | done |
| 152 | Bandai 74*161/161/32 (single-screen variant; Saint Seiya: Ougon Densetsu Kanketsu Hen, Pocket Zaurus, Tigers no Kessho-ban, Arkanoid II) | done |
| 184 | Sunsoft-1 (Atlantis no Nazo, Wing of Madoola, Hi no Tori, Maharaja, Kid Niki JP, Ripple Island; CHR-only banking with hardware-forced bit 7 on the high slot) | done |
| 185 | CNROM with diode-array security (B-Wings, Mighty Bomb Jack, Spelunker, Seicross, Sansuu series; sub 0 heuristic + deterministic subs 4-7) | done |
| 154 | Namco 118 / Devil World JP (dynamic single-screen mirroring + Type C CHR wiring) | done |
| 159 | Bandai LZ93D50 + 24C01 | done |
| 206 | Namco 118 / Mimic-1 (DigDug II, Mappy-Land, Galaxian) | done |
| 207 | Taito X1-005 alt-mirroring variant (Fudou Myou-Ou Den) | done |
| 232 | Codemasters / Camerica BF9096 (Quattro Adventure / Arcade / Sports multicart; sub 1 = Aladdin Deck Enhancer bit-swapped outer block) | done |

### Test-ROM coverage

All ROMs in these suites pass:

| Category | Suite | Result |
|---|---|---|
| CPU | `instr_test-v5`, `instr_test-v3`, `instr_misc`, `instr_timing`, `nes_instr_test`, `cpu_dummy_reads`, `cpu_dummy_writes`, `cpu_exec_space`, `cpu_reset`, `blargg_nes_cpu_test5`, `cpu_interrupts_v2`, `cpu_timing_test6`, `branch_timing_tests` | pass |
| APU | `apu_test` (8/8), `apu_reset` (6/6), `blargg_apu_2005` (11/11), `dmc_dma_during_read4` (5/5, strict pattern), `sprdma_and_dmc_dma{,_512}` (2/2) | pass |
| PPU | `sprite_hit_tests_2005` (11/11), `sprite_overflow_tests` (5/5), `ppu_vbl_nmi` (10/10), `oam_read`, `oam_stress`, `ppu_read_buffer`, `ppu_open_bus`, `blargg_ppu_tests_2005.09.15b` (4/5, see below) | pass |
| MMC3 | `mmc3_test` (6/6), `mmc3_test_2` (6/6), `mmc3_irq_tests` (6/6) | pass |
| Save state | unit + frame-level integration: capture/apply round-trip, encode + decode + apply to fresh Nes, run-after-restore byte-equal, framebuffer byte-equal 30 frames past round-trip, mapper-variant rollback | pass |

### Known gaps

- **Second controller + rebinding.** Player 1 is wired to the
  keyboard; player 2 and configurable bindings are future work.
- **`blargg_ppu_tests_2005.09.15b/power_up_palette`.** Won't fix.
  Compares the power-on palette byte-for-byte against values captured
  from blargg's specific NES unit; passing requires hardcoding that
  unit's power-on contents, which isn't hardware behavior worth
  reproducing.

## Building and running

```
cargo build --release
./target/release/vibenes [path/to/rom.nes]
```

The binary can launch without a ROM; use the overlay's File menu to
load one. Current region (NTSC/PAL) is detected from the iNES header
and the built-in CRC32 game DB, and the host audio sample rate is
matched to it.

**Keys**: `Z`=B, `X`=A, `Enter`=Start, `RShift`=Select, arrows=D-pad,
`R`=reset, `F1`=overlay menu, `F2`=save state to active slot, `F3`=load
state from active slot, `0`-`9`=select active save-state slot,
`F4`=FDS disk swap, `F12`=debug submenu, `Esc`=back/quit.

**Gamepad** (P1, fixed mapping for now - remapping UI is future
work): Xbox-style `A`=A, `X`=B, `Back`=Select, `Menu/Start`=Start,
D-pad or left stick for the D-pad. `Home`/`Guide` toggles the overlay
menu; while the menu is open, D-pad up/down moves the cursor, `A`
confirms, `B` backs out. Keyboard and gamepad state OR together, so
either works at any time. Controller 2 is still future work.

The overlay menu (F1) pauses the emulator and shows a centered modal
over a darkened freeze-frame: Scale (1x-6x), Aspect (Auto / 1:1 / 5:4
/ 8:7 NTSC / 11:8 PAL), Recent ROMs, Load ROM, Reset, Quit. Navigate
with arrows / Enter / Esc or the mouse.

## Saves

Cartridges with battery-backed PRG-RAM (iNES flag6 bit 1, or the
NES 2.0 `prg_nvram_size` byte) persist their RAM to
`~/.config/vibenes/saves/<rom-stem>.sav` (respects
`$XDG_CONFIG_HOME`). The save is written atomically (temp file +
rename) so a crash mid-write leaves either the old save or the new
one, never a torn file.

Alternative layouts are selectable via [`SaveStyle`](src/config.rs):
- `NextToRom` writes `<rom-dir>/<rom-stem>.sav`.
- `ByCrc` writes `<saves-dir>/<PRG+CHR CRC32>.sav`, which survives ROM
  renames.

Today the selection is a compile-time default; a settings UI is
planned.

Flush triggers:

1. App quit (window close and the F1 -> Quit menu item).
2. ROM swap (outgoing cart flushes before the new one loads).
3. Periodic safety flush every ~3 minutes of emulated time
   (10800 frames at 60 Hz). This only narrows the SIGKILL
   data-loss window; the quit/swap triggers above are the
   authoritative ones.

Battery RAM on real hardware is just SRAM; the game has no
"save commit" signal to latch on. Writes buffer in memory and
flush at session boundaries. `src/bin/battery_probe <rom>` is a
diagnostic that exercises the full load/write/save/reload pipeline
on any ROM so you can verify the save path end-to-end without
reaching the in-game save trigger.

Non-battery cartridges produce no save files.

FDS disks save as an IPS sidecar: writes performed by the game are
captured as a delta against the original `.fds` image and written to
`<rom-stem>.ips` next to the save path. On reload, the sidecar is
applied over the pristine disk image, so the on-disk `.fds` stays
untouched and the save is portable.

## Save states

Save states snapshot the entire emulation state to a slot file. Hit
`F2` to save, `F3` to load, and the bare digit keys `0`-`9` to pick
which of the 10 slots is active. The active slot persists across
launches in `settings.kv` so you can keep working with the same slot
across sessions.

Slot files live alongside battery saves at
`~/.config/vibenes/saves/<rom-stem>.<crc>.<region>.state<N>` (or in
the rom directory under `SaveStyle::NextToRom`). Embedding both the
ROM CRC32 and the region tag in the path keeps three classes of
collision from clobbering each other:

- NTSC and PAL builds of the same game (often identical PRG/CHR
  binaries with different iNES region flags).
- An IPS-patched hack, fan translation, or revision against the
  base ROM with the same filename.
- Any rename to a name another ROM already used.

Cross-ROM, cross-mapper, and cross-region loads are caught by the
file header and rejected before any state is touched. If the apply
itself fails partway, an in-memory backup captured at load time
restores the live state byte-for-byte (puNES rollback pattern).

Mapper coverage: NROM, MMC1, UxROM, CNROM, MMC3 (incl. MMC6),
MMC5, AxROM, MMC2, MMC4, GxROM, the VRC1/2/3/4/6/7 family, FME-7
(incl. Sunsoft 5B audio), Bandai FCG (incl. EEPROM), Jaleco
SS88006, Jaleco JF-17/JF-19, Namco 163 (incl. wavetable audio),
RAMBO-1, Irem G-101, Irem H3001, Irem 74*161, Irem TAM-S1,
Bandai 74*161, Sunsoft-1, Sunsoft-2, BNROM/NINA-001, Taito
TC0190, Mapper 037, FDS (incl. RP2C33 audio + disk-side state). VRC7 OPLL state is replayed through emu2413 from a
register-file shadow so the chip is fully restored without
freezing the format around the C struct.

What's not in scope: rewind buffers, runahead, mid-instruction or
mid-DMA snapshots, screenshot thumbnails embedded in slot files,
and migration between save-state format versions. The current
format version is `1`; loading a future-version state into an
older build fails cleanly rather than silently corrupting.

## Settings

Runtime settings live in [`src/config.rs`](src/config.rs) as plain
Rust defaults. The user-tunable subset that the in-game overlay
already exposes is persisted across launches in
`~/.config/vibenes/settings.kv` (respects `$XDG_CONFIG_HOME`), a
tiny `key=value` file managed by [`src/settings.rs`](src/settings.rs).
Today the integer scale and the active save-state slot survive a
restart; more fields move out of `config.rs` and into the
persisted file as the settings UI grows.

## Testing

```
# Unit tests + integration suites
cargo test --release

# Headless blargg runners (for ROMs not in the integration suites)
./target/release/test_runner ROM.nes          # $6000/DE-B0-61 protocol
./target/release/blargg_2005_report ROM.nes   # pre-$6000 nametable scan
```

`test_runner` handles the standard blargg `$6000` status-byte protocol
including the `$81` reset request. `blargg_2005_report` watches for the
CPU trapping in a `forever:` loop and scans nametable 0 for a result,
recognizing `$hh` debug bytes (2005-era devcart loader), ca65 framework
keywords (`Passed` / `Failed` / `Error N`), blargg keywords (`PASSED`
/ `FAILED` / `FAIL OP`), and `All tests complete`.

Integration test suites gate against curated ROM sets:
- `tests/blargg_apu_2005.rs` for the 11-ROM 2005 APU suite.
- `tests/dmc_dma_during_read4.rs` for the 5 DMC/DMA interaction ROMs,
  strict-pattern (golden CRC `F0AB808C` on `dma_4016_read`,
  sanctioned `5E3DF9C4` on `dma_2007_read`).
- `tests/battery_save.rs` for a synthetic NROM battery cart; writes
  PRG-RAM via the bus, saves, drops the Nes, reloads, verifies
  persistence; asserts non-battery carts never create a `.sav`.
- `tests/save_state_integration.rs` for end-to-end save-state
  correctness on a synthetic in-memory NROM cart: capture/apply
  is a no-op, encode + decode + apply to a fresh Nes is byte-equal
  to the source, run-after-restore matches a continuous run, and
  the framebuffer is byte-identical 30 frames past a round trip.

## License

Licensed under the GNU General Public License v3.0 or later. See
[`LICENSE`](LICENSE) for the full text.

This project uses small amounts of code ported from Mesen2
(GPL-3.0-or-later) in the FDS audio synth, VRC6 audio, and FDS disk
image rebuild. Those ports are attributed inline in the relevant
source files and in the commit history. All other subsystems are
re-implementations written against the public NES hardware
documentation and behavioral observation of reference emulators.

The VRC7 (mapper 85) FM audio backend bundles
[emu2413](https://github.com/digital-sound-antiques/emu2413) v1.5.9 by
Mitsutaka Okazaki under [`vendor/emu2413/`](vendor/emu2413/), used
unmodified under its MIT license - see the file there for the full
notice.

## Credits and references

- [Mesen2](https://github.com/SourMesen/Mesen2) by Sour -
  primary behavioral reference, and the source of the ported FDS and
  VRC6 audio code noted above. GPL-3.0-or-later.
- [puNES](https://github.com/punesemu/puNES) by FHorse -
  secondary reference, especially for the DMC/DMA interleave.
  GPL-2.0-or-later.
- [Nestopia UE](https://github.com/rdanbrook/nestopia) by
  R. Danbrook (fork of Martin Freij's Nestopia) - tertiary
  reference for CPU/DMA edge cases. GPL-2.0-or-later.
- [emu2413](https://github.com/digital-sound-antiques/emu2413) by
  Mitsutaka Okazaki - YM2413/VRC7 OPLL FM core, vendored verbatim
  under [`vendor/emu2413/`](vendor/emu2413/). MIT.
- [NESdev Wiki](https://www.nesdev.org/wiki/Nesdev_Wiki) and the
  [blargg test ROMs](https://github.com/christopherpow/nes-test-roms)
  underpin essentially every subsystem.
