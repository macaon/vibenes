# PPUSTATUS read timing (2026-11-09)

## Evidence and scope

[NESdev's programmer reference](https://www.nesdev.org/wiki/PPU_programmer_reference)
places the automatic clearing of all three status flags at pre-render
dot 1. This change preserves that edge.

The [Visual 2C02 circuit tutorial](https://www.nesdev.org/wiki/Visual_circuit_tutorial)
describes the VBlank read buffer: the value is isolated and held when
the status read is asserted, allowing the CPU to read it after the
underlying flag clears. It also describes CPU data sampling at the end
of phase 2. This independently supports separating read assertion from
CPU sampling; it does not establish every sprite-flag propagation delay.

The matching [AccuracyCoin source](https://github.com/100thCoin/AccuracyCoin/blob/14b978badcc98920844de046cede5dc4465db2d6/AccuracyCoin.asm)
explains that the sprite flags remain live during the read, whereas
VBlank is buffered early. Its clearing subtest accepts two alignments.
Combining this explanation with the circuit evidence supports sampling
sprite flags later rather than moving their actual clear to dot 0.
No new hardware captures were made.

## Change

Previously the CPU read sampled all three flags at assertion. The bus
now retains the buffered VBlank value, executes the existing post-access
PPU ticks, then samples live sprite flags. It updates both CPU bus
latches and the sprite bits of the PPU I/O latch with the final value.
Normal reads, mirrored registers, page-cross dummy reads, DMA halt/idle
reads, and OAM DMA reads use the same completion path.

VBlank clearing, write-toggle reset, NMI suppression, clock counts, and
PPU flag generation retain their existing timing. No persistent state
was added; save-state format remains v5.

A boundary regression checks reads before, across, and after pre-render
dot 1 through all four paths. It verifies the mixed result (buffered
VBlank with cleared sprite flags), latch contents, side effects, and
the unchanged three-dot read duration.

## Results and next investigation

The clearing subtest now produces `E0 E0 80 00`, matching the primary
answer key. `$2002 flag timing` advances from **FAIL 1 to FAIL 2**.
Overall AccuracyCoin remains **134 pass / 13 fail**, with no other
result changes. See [the full report](results-2026-11-09-status-read.txt).

Temporary diagnostics captured the flag-setting samples as
`00 00 40 60 60 60 60` (zero-page `$51..$57`). Subtest 2 expects `$53=00`,
`$54=00` or `40`, and `$55=60`. The remaining discrepancy concerns flag
generation or its visibility within the PPU dot, rather than the
pre-render clear. The renderer currently raises sprite-zero hit while
processing pixel x at dot x+1; the [PPU rendering reference](https://www.nesdev.org/wiki/PPU_rendering)
places its earliest visibility at dot 2. Overflow visibility also needs
checking. Those pipelines need separate evidence and tests before
changing their timing; no blanket delay was added to satisfy this ROM.

Validation: 1,387 library tests, four save-state integration tests,
11 Blargg APU integration tests, five strict DMC/read integration tests,
the 44-ROM CPU/APU/reset/DMA/PPU-NMI/MMC3 sweep, and all 11 legacy
sprite-hit plus five sprite-overflow ROMs pass. The legacy sprite ROMs
were read using `blargg_2005_report`, since they have no `$6000` protocol.
