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
- APU with 5 channels, frame counter, DMC
- Expansion audio via bus-level mixer with per-chip pre-scaled blend
- Host audio via cpal + blip_buf
- Windowed runtime on wgpu, NTSC/PAL-paced, keyboard input
- Overlay UI (egui, F1)
- Battery-backed saves with atomic writes and flush-on-quit/swap
- Famicom Disk System (mapper 20) with BIOS, disk swap, IPS sidecar
  saves, RP2C33 audio

### Supported mappers

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
| 19 / 210 | Namco 163 / 175 / 340 | done (audio DSP deferred) |
| 20 | Famicom Disk System | done |
| 24 | Konami VRC6a | done |
| 26 | Konami VRC6b | done |
| 66 | GxROM / MHROM | done |
| 159 | Bandai LZ93D50 + 24C01 | done |

### Test-ROM coverage

All ROMs in these suites pass:

| Category | Suite | Result |
|---|---|---|
| CPU | `instr_test-v5`, `instr_test-v3`, `instr_misc`, `instr_timing`, `nes_instr_test`, `cpu_dummy_reads`, `cpu_dummy_writes`, `cpu_exec_space`, `cpu_reset`, `blargg_nes_cpu_test5`, `cpu_interrupts_v2`, `cpu_timing_test6`, `branch_timing_tests` | pass |
| APU | `apu_test` (8/8), `apu_reset` (6/6), `blargg_apu_2005` (11/11), `dmc_dma_during_read4` (5/5, strict pattern), `sprdma_and_dmc_dma{,_512}` (2/2) | pass |
| PPU | `sprite_hit_tests_2005` (11/11), `sprite_overflow_tests` (5/5), `ppu_vbl_nmi` (10/10), `oam_read`, `oam_stress`, `ppu_read_buffer`, `ppu_open_bus`, `blargg_ppu_tests_2005.09.15b` (4/5, see below) | pass |
| MMC3 | `mmc3_test` (6/6), `mmc3_test_2` (6/6), `mmc3_irq_tests` (6/6) | pass |

### Known gaps

- **Additional mappers.** VRC2 / VRC4 / VRC7 are the remaining Konami
  gaps; Sunsoft 5B (mapper 69, used by Gimmick!) is another high-value
  unlock. All of them plug into the existing `Mapper::audio_output`
  expansion-audio mixer.
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
`R`=reset, `F1`=overlay menu, `F4`=FDS disk swap, `Esc`=back/quit.

**Gamepad** (P1, fixed mapping for now — remapping UI is future
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

Runtime settings live in [`src/config.rs`](src/config.rs) as plain
Rust defaults. The user-tunable subset that the in-game overlay
already exposes is persisted across launches in
`~/.config/vibenes/settings.kv` (respects `$XDG_CONFIG_HOME`) — a
tiny `key=value` file managed by [`src/settings.rs`](src/settings.rs).
Today only the integer scale survives a restart; more fields move
out of `config.rs` and into the persisted file as the settings UI
grows.

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

## Layout

```
src/
  main.rs, app.rs             windowed binary + shared glue
  bus.rs, clock.rs            CPU bus + master clock
  cpu/{mod,flags,ops}.rs      6502 core, status, all opcodes
  ppu.rs                      2C02 render pipeline
  apu/                        pulse x 2, triangle, noise, DMC,
                              frame counter, envelope, sweep,
                              length counter
  mapper/                     NROM, MMC1-5, UxROM, CNROM, AxROM,
                              MMC2/4, Bandai FCG + 24C0x EEPROM,
                              Jaleco SS88006, Namco 163/175/340,
                              FDS (mapper 20), VRC6 (24/26), GxROM.
                              Expansion audio: fds_audio,
                              vrc6_audio. Shared FDS-side transport
                              in src/fds/ (image, ips, bios).
  gfx/                        wgpu renderer + wgsl passthrough
  ui/                         egui overlay (menus, commands,
                              recent ROMs)
  audio.rs                    cpal + blip_buf
  video.rs                    scale + pixel-aspect settings
  gamedb.rs, crc32.rs         CRC32-keyed region/chip DB
  nes.rs, rom.rs              system glue + iNES parser
  blargg_2005_scan.rs         stuck-PC + nametable scanner
  bin/
    test_runner.rs            $6000-protocol runner
    blargg_2005_report.rs     pre-$6000-protocol runner
    frame_dump.rs             framebuffer PNG dump
    dma_4016_dump.rs          DMC/DMA ROM nametable dumper

tests/
  blargg_apu_2005.rs          APU suite (11 ROMs)
  dmc_dma_during_read4.rs     DMC/DMA suite (5 ROMs)

assets/fonts/                 VT323 pixel font (SIL OFL) for
                              the overlay menu
```

## License

Licensed under the GNU General Public License v3.0 or later. See
[`LICENSE`](LICENSE) for the full text.

This project uses small amounts of code ported from Mesen2
(GPL-3.0-or-later) in the FDS audio synth, VRC6 audio, and FDS disk
image rebuild. Those ports are attributed inline in the relevant
source files and in the commit history. All other subsystems are
re-implementations written against the public NES hardware
documentation and behavioral observation of reference emulators.

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
- [NESdev Wiki](https://www.nesdev.org/wiki/Nesdev_Wiki) and the
  [blargg test ROMs](https://github.com/christopherpow/nes-test-roms)
  underpin essentially every subsystem.
