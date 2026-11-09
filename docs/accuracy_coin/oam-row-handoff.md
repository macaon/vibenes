# OAM row handoff (2026-11-09)

## Hardware basis

[NESdev's OAM internals](https://www.nesdev.org/wiki/OAM_internals)
describes 32 physical DRAM rows, each containing eight primary-OAM
bytes and one secondary-OAM byte. Switching rows without precharging
the shared bitlines can copy the old row into the new one.

The [hardware errata](https://www.nesdev.org/wiki/Errata) identifies a
deterministic instance on C/E/G/H NTSC PPUs: at pre-render dot 0, the
address switches from OAMADDR to the retained secondary-OAM row and
copies the former row into the latter. Source row 0 is common because
OAMADDR is normally zero, but it is not the only possible source.
PAL's 2C07 is not assigned this NTSC corruption behavior here.

[Fiskbit's research](https://forums.nesdev.org/viewtopic.php?p=249339)
describes the retained row after disabling during secondary-OAM clear
and sprite fetch. [PPU signals](https://www.nesdev.org/wiki/PPU_signals)
documents delayed control signals and the extra pointer increment at
dot 321 that leaves a safe zero row during sprite idle.

## Independent pointer probe and implementation references

The [Visual 2C02 probe](probes/visual2c02-oam-retention.cjs) initializes
OAM through the simulator's memory helper, enables background rendering
through the register pins, and disables it starting at scanline 0 dot
7 with a 15-master-half-cycle M2-high pulse. After release, the low five
`spr_ptr` bits reach 4 and remain there. The probe runs the transistor
netlist, not AccuracyCoin or an emulator's evaluation algorithm.

Run it against the same eight public files listed in the
[overflow investigation](sprite-overflow-output-latch.md):

```sh
node docs/accuracy_coin/probes/visual2c02-oam-retention.cjs /path/to/visual2c02
```

[The trace](probes/visual2c02-oam-retention.txt) records source hashes. This is a controlled phase experiment
for pointer retention; it does not simulate an entire CPU, prove every
CPU/PPU alignment, or directly capture the next frame's row copy. The
pre-render copy itself is supported by the hardware errata above. No
new physical-console measurements were made.

At the user's suggestion, shallow reference checkouts were downloaded
to persistent storage outside this repository:

- `~/Git/ref/Mesen2`: `b9fa69ddc6d0a331fb103fdb5eef6904305703c2`
- `~/Git/ref/puNES`: `b1758ec26a3d6da10abf50dc184fa837236c5f61`

[Mesen2's PPU update](https://github.com/SourMesen/Mesen2/blob/b9fa69ddc6d0a331fb103fdb5eef6904305703c2/Core/NES/NesPpu.cpp)
has two rendering-enable stages and explicitly labels its optional
row-corruption approximation incomplete. That timing distinction was
useful; its array of corruption flags and row-zero-only copy were not
adopted. PuNES was downloaded for future comparisons; this change does
not rely on a PuNES corruption implementation.

## Change and scope

The existing evaluation byte count was insufficient to identify the
physical row: it did not track clear/fetch progression or retain a row
through forced blank. A separate five-bit selector now tracks those
phases and freezes with a second rendering-control stage. At NTSC
pre-render dot 0, rendering switches from the current OAMADDR source
row to that retained target. All nine bytes are copied, including the
secondary byte. No copy is performed merely because PPUMASK was written
or rendering was enabled during vblank.

This is a focused implementation of the deterministic pre-render
handoff. It does not implement all corruption from OAMADDR writes,
arbitrary mid-scanline rendering toggles, analog effects, all PPU
revisions, or the complete delayed OAM evaluation pipeline. Those remain
accuracy limits; passing this ROM is not evidence of complete OAM
electrical fidelity. Existing pixel, fetch, IRQ, and flag timings are
unchanged.

The retained selector and extra rendering stage are serialized in
save-state format **v8**. Older states are rejected; battery saves are
unaffected.

## Validation

AccuracyCoin OAM Corruption changes from FAIL 2 to PASS, including the
checks that corruption is absent immediately after disabling and after
enabling during vblank. Overall: **136 pass / 11 fail / 0 skip / 0
untouched**, with no other scored changes. See
[the full report](results-2026-11-09-oam-corruption.txt).

New regressions check all nine bytes, a nonzero OAMADDR source row,
unmodified surrounding rows, the PAL exclusion, delayed row retention,
restore through vblank, the safe idle row, and keeping one rendering
component enabled. All 1,393 library tests, 20 integration tests, and
the 60-ROM CPU/APU/reset/DMA/PPU-NMI/MMC3/sprite-hit/overflow sweep pass.
