# OAM reads after primary-address wrap (2026-11-10)

## First mismatch

The `$2004 Stress Test` descending-data fixture first diverged at sample
206: the emulator returned `$FF`, while the expected value was `$03`.
All samples before that agreed. There were 26 mismatches, on alternating
samples through the remainder of evaluation. The ROM recorded failing
index `$CE` in zero page `$21`.

By that point all 64 primary entries have been scanned, but only two
sprites were copied, so secondary OAM contains eight selected bytes.
The final rejected Y byte remains at secondary address 8. Once the
primary cursor wraps, writes are disabled and even evaluation cycles
read that secondary location. Odd cycles continue reading primary OAM.

The emulator already implemented this readback when secondary OAM was
full, but incorrectly required fullness in the drained-primary branch
too. Removing that guard restores the alternating data. No counters,
clock edges, sprite selections, flag timing, or fetch timing changed.

## Independent evidence

[NESdev's sprite-evaluation description](https://www.nesdev.org/wiki/PPU_sprite_evaluation)
documents alternating primary reads and secondary accesses, and that a
disabled secondary write becomes a read. To check the primary-wrap
case independently of the ROM's answer key, we ran the public
[Visual 2C02 transistor simulator](https://www.qmtpro.com/~nes/chipimages/visual2c02/).

[The probe](probes/visual2c02-oam-wrap.cjs) initializes two in-range
sprites with zero-valued bytes, fills the remaining primary entries
with `$FF`, and changes the final sprite's rejected Y byte to `$AA`.
Thus only eight secondary bytes are occupied and secondary byte 8
retains `$AA`. After enabling rendering, the netlist is clocked to the
wrap window on visible line 0. A held OAMDATA read exposes the pins
without changing the OAM address or evaluation sequence.

The observed I/O data alternates between primary values (`$00`, then
`$FF` as the cursor advances) and secondary `$AA`, while the secondary
address remains 8. The harness asserts this sequence at chip hpos
206–211. The emulator regression labels the processed dot; the chip
trace labels the current counter phase. The comparison establishes
the alternating source selection, rather than introducing a change to
the existing CPU/PPU phase convention.

The probe uses an extended read-select window to observe the live bus;
it does not simulate a complete CPU or claim a physical console capture.
It directly initializes memory through the simulator's helper. The
eight public input files and their hashes are recorded in
[the trace](probes/visual2c02-oam-wrap.txt). Reproduce using the inputs
listed in the [overflow investigation](sprite-overflow-output-latch.md):

```sh
node docs/accuracy_coin/probes/visual2c02-oam-wrap.cjs /path/to/visual2c02
```

The downloaded Mesen2 source includes configuration-dependent sprite
evaluation branches, one of which retains the same secondary-full
guard in its drained path. The independent chip result, rather than
agreement with another emulator implementation, supports this fix.

## Validation

A regression runs the real evaluator until primary wrap with two
selected sprites, checks the partial secondary occupancy and rejected
Y byte, then serializes and restores before checking the alternating
reads. Secondary memory must remain unchanged throughout those reads.

Both AccuracyCoin stress datasets now pass: the descending fixture
with primary wrap and the fixture that fills secondary OAM and exercises
the diagonal overflow scan. The suite reports **138 pass / 9 fail / 0
skip / 0 untouched**. Only `$2004 Stress Test` changes from the prior
report. See [the full results](results-2026-11-10-oam-primary-wrap.txt).

All 1,399 library tests, 20 integration tests, and the 60-ROM
CPU/APU/reset/DMA/PPU-NMI/MMC3/sprite-hit/overflow sweep pass. Existing
serialized evaluation state suffices; save-state format remains v8.
Temporary ROM diagnostics were removed.
