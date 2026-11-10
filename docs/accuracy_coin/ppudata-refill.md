# Delayed PPUDATA operations (2026-11-10)

## Diagnosis

The first stable mismatch in `$2007 Stress Test` was sample 3: `$02`
instead of attribute byte `$C0`. The first several samples repeated
nametable values because reads immediately refilled the buffer from
`v`. That bypassed the external bus operation and the rendering fetch
actually in progress.

[The register reference](https://www.nesdev.org/wiki/PPU_programmer_reference)
explains why PPUDATA has a read buffer: the external access cannot
complete within the CPU read. Palette reads return internal palette
data immediately while a separate external access refills the buffer.
[The pinout](https://www.nesdev.org/wiki/PPU_pinout) and
[rendering reference](https://www.nesdev.org/wiki/PPU_rendering)
describe the shared low-address/data pins and two-dot memory fetches.
These support modeling an operation after register access, rather than
an immediate second memory read during rendering.

## Independent timing probes

The [read probe](probes/visual2c02-ppudata-read.cjs) runs the public
[Visual 2C02 netlist](https://www.qmtpro.com/~nes/chipimages/visual2c02/)
with rendering disabled, sets `v=$2C00` through register pins, and
supplies external memory byte `low(address) XOR $A5`. It asserts a
PPUDATA read for 15 master half-cycles, releases select, then traces
ALE, RD, the buffer, and `v`. The old buffer remains visible during
the CPU read. At the default probe phase, after release:

- ALE starts after 9 master half-cycles.
- The buffer receives `$A5` after 25 half-cycles.
- `v` advances to `$2C01` after 29 half-cycles.

Four probe phase offsets (0, 2, 4, 6 half-cycles) produce refill times
25, 23, 21, 27 and increment times 29, 27, 25, 31 respectively. This
confirms both the delayed operation and its phase sensitivity; it is
not evidence that one integer delay models every half-cycle.
[The read traces](probes/visual2c02-ppudata-read.txt) include source hashes.

The [zero-byte write timing probe](probes/visual2c02-ppudata-write.cjs)
similarly observes delayed ALE/WR and the external write to `$2C00`,
followed by the `v` increment. It checks timing and address with a zero
payload; nonzero write behavior is covered by emulator regressions and
the hardware-tested Blargg ROM. See
[the write trace](probes/visual2c02-ppudata-write.txt).

Use the eight simulator files listed in the
[overflow investigation](sprite-overflow-output-latch.md):

```sh
node docs/accuracy_coin/probes/visual2c02-ppudata-read.cjs /path/to/visual2c02 0
node docs/accuracy_coin/probes/visual2c02-ppudata-read.cjs /path/to/visual2c02 2
node docs/accuracy_coin/probes/visual2c02-ppudata-read.cjs /path/to/visual2c02 4
node docs/accuracy_coin/probes/visual2c02-ppudata-read.cjs /path/to/visual2c02 6
node docs/accuracy_coin/probes/visual2c02-ppudata-write.cjs /path/to/visual2c02
```

These are synthetic pin-level transistor-model experiments, not new
physical-console captures or a complete CPU simulation.

## Implementation

Reads now return the previous buffer (or the immediate palette value)
and arm an external request. CPU select release starts a whole-dot
sequence: capture the address in the address-latch phase, then complete
the refill and address increment later. In the current phase model,
completion occurs after four PPU ticks. During rendering, stable
refills sample the rendering fetch byte; during blanking they read the
captured address. Ordinary rendering fetches do not continuously refill
the CPU read buffer.

Writes have a corresponding pending operation and address capture.
This matters when an indexed-store dummy read immediately precedes a
write: the read must advance `v` before the following write captures
its address. The initial read-only prototype failed Blargg's
`read_write_2007`; implementing the write sequence restored its exact
expected result. The old special case that suppressed PPUDATA dummy
read effects is removed. Dummy and ordinary reads use the request gate;
an overlapping request does not restart an active operation.

Two fetch corrections are also required for the stable rendering-bus
bytes. Each sprite slot performs two garbage nametable reads, not a
nametable read followed by an attribute read. At dot 257 the first
garbage fetch uses `v` before the horizontal `t`-to-`v` copy. These agree
with [the rendering discussion](https://www.nesdev.org/wiki/Talk%3APPU_rendering)
and the delayed controls in [PPU signals](https://www.nesdev.org/wiki/PPU_signals).
Mapper address notification remains at the existing rendering-fetch
phase; no CPU or DMA cycles are added.

Pending addresses, data, phase counters, release gates, and the last
external fetch byte survive save/restore in format **v9**. Older states
are rejected. Battery saves are unchanged.

## Validation and limits

AccuracyCoin now reports **139 pass / 8 fail / 0 skip / 0 untouched**.
Only `$2007 Stress Test` changes from the previous report; all 170
stable samples pass. See [the full results](results-2026-11-10-ppudata.txt).
The ROM explicitly excludes alternating samples affected by analog
ALE/read feedback. Passing it does not validate those samples.

All 1,405 library tests, 20 integration tests, and 62 regression ROMs
pass. The ROM sweep adds `test_ppu_read_buffer` and `ppu_open_bus` to the
previous 60. Strict DMC/read tests include the rapid read/write case
that exposed the incomplete prototype. Unit regressions cover delayed
blanking and palette refills, overlapping accesses, read-then-write
ordering, sprite garbage fetches, and restoring pending reads/writes
and the retained fetch byte immediately before completion.

This remains a whole-dot stable-bus model. It does not implement the
analog feedback loop, a complete external ALE/RD/WR waveform, all hybrid
addresses, every overlapping-write corner, or establish half-cycle
accuracy across all CPU/PPU alignments and PAL revisions. The existing
ALE + Read and Hybrid Addresses failures remain. Temporary ROM
diagnostics were removed; no answer-key lookup is used by the emulator.
