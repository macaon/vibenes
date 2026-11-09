# Sprite-overflow output latch (2026-11-09)

## Independent chip evidence

The previous sprite-zero correction left `$2002 flag timing` at FAIL 2:
overflow was already visible in a sample that should contain at most
sprite-zero hit. Rather than infer an arbitrary delay from this answer,
we ran [Quietust's Visual 2C02 transistor simulator](https://www.qmtpro.com/~nes/chipimages/visual2c02/)
with controlled OAM contents.

The [probe harness](probes/visual2c02-overflow.cjs) loads the unmodified
public simulator, netlist, reset state, and OAM helpers. It starts from
the supplied even-frame pre-render state; fills OAM through the helper;
writes `$08` to PPUMASK through the I/O pins; supplies zero CHR reads;
and clocks the actual netlist. It records the first visible scanline,
including both phases of dots 126–134. No CPU or AccuracyCoin code runs
inside this probe, and no internal evaluation or flag nodes are forced.
The OAM helper directly initializes the memory cells; this does not
test the CPU OAM write path.

Fixtures put zero in all four bytes of each selected sprite and `$FF`
in all other entries. The first eight entries are selected. The ninth,
when present, is either adjacent or follows one skipped entry:

| Fixture | First `spr_overflow` rise |
| --- | --- |
| [Eight selected sprites](probes/visual2c02-overflow-8-0.txt) | None through dot 152 |
| [Nine adjacent selected sprites](probes/visual2c02-overflow-9-0.txt) | Dot 131, `pclk0` |
| [Ninth selected entry after one gap](probes/visual2c02-overflow-9-1.txt) | Dot 133, `pclk0` |

The gap fixture also exercises the diagonal overflow scan: the tile
byte of the following entry can be treated as a Y coordinate. It is
not evidence that this path compares only actual sprite Y coordinates.

[The nine-sprite trace](probes/visual2c02-overflow-9-0.txt) includes SHA-256
hashes of all eight inputs. In that netlist, transistor `t2923` connects
`/spr_overflow` to ground when `sec_oam_overflow` is asserted. That pulse
is gated by `pclk1`, the buffered comparison result, the primary-OAM
overflow state, and delayed horizontal parity (`t2926`, `t2751`,
`t2845`, `t2890`). It rises on dot 131 after the ninth candidate's
read/compare pair at 129/130. Thus the comparison and the output flag
are distinct events. This is transistor-model evidence, not a new
physical-console capture; the simulator's own electrical limitations
still apply.

To reproduce, obtain these eight files from the linked simulator into
one directory: `nodenames.js`, `segdefs.js`, `transdefs.js`, `wires.js`,
`chipsim.js`, `macros.js`, `states.js`, `sprtable.js`. No source copies
are vendored in this repository. Run:

```sh
node docs/accuracy_coin/probes/visual2c02-overflow.cjs /path/to/visual2c02 8 0
node docs/accuracy_coin/probes/visual2c02-overflow.cjs /path/to/visual2c02 9 0
node docs/accuracy_coin/probes/visual2c02-overflow.cjs /path/to/visual2c02 9 1
```

## Emulator change

The evaluator processed the ninth comparison at dot 130 and immediately
set status bit 5. It now records a pending result, which reaches the
overflow flag on the next dot. The read/copy schedule, diagonal-scan
logic, and pre-render dot-1 clearing stay unchanged. Reset discards a
pending result, and the pre-render clear takes precedence over it.

A new regression runs all three fixtures through the real evaluation
loop and checks the first flag rise against the chip probe. It also
encodes, decodes, and restores a pending comparison immediately before
visibility. The existing reset/pre-render regression covers both
pending sprite flags. The new pending bit requires save-state format
**v7**; older states are rejected. Battery saves are unaffected.

## Results

`$2002 flag timing` now passes both clearing and setting subtests.
AccuracyCoin reports **135 pass / 12 fail / 0 skip / 0 untouched**;
no other result changes. See [the full report](results-2026-11-09-overflow.txt).

Validation: all 1,390 library tests, four save-state integration tests,
11 Blargg APU and five strict DMC/read integrations, the 44-ROM
CPU/APU/reset/DMA/PPU-NMI/MMC3 sweep, and all 16 legacy sprite-hit/overflow
ROMs pass.
