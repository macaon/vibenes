# DMC enable gate with a buffered byte (2026-11-09)

## Evidence and diagnosis

[NESdev DMA](https://www.nesdev.org/wiki/DMA#DMC_DMA) distinguishes
load and reload DMA and documents the delayed halt attempt after a
`$4015` enable. The matching
[AccuracyCoin source](https://github.com/100thCoin/AccuracyCoin/blob/14b978badcc98920844de046cede5dc4465db2d6/AccuracyCoin.asm)
(`TEST_DeltaModulationChannel`, subtests L-N) exercises the collision
between enabling DMC and a buffered byte being consumed. Our first
failure was encoded error `$15` (subtest L).

The implementation armed the enable delay only when the sample buffer
was empty at the instant of the write. With an occupied buffer, the
delay stayed zero. Consuming that buffer shortly afterward therefore
requested a refill immediately, bypassing the enable gate entirely.

[Mesen2's SetEnabled/ProcessClock behavior](https://github.com/SourMesen/Mesen2/blob/master/Core/NES/APU/DeltaModulationChannel.cpp)
provides a separate implementation reference: the enable delay is
started when a sample restarts regardless of buffer occupancy, while
buffer emptiness is checked when the gate expires. This is not an
independent hardware measurement. The exact collision cases here rely
on AccuracyCoin's hardware test expectations; no new console captures
were made.

## Change

Every restart through `$4015` now arms the existing enable delay,
even if a sample byte is buffered. If the output unit drains that byte
before the gate opens, its refill waits. If the buffer remains full
when the gate opens, no DMA occurs until it is actually consumed.

The delay constants and parity convention are unchanged. An already
active sample is not restarted by another enable write. Normal output
and buffer-consumption clocks are unchanged, and there is no CPU
revision switch or injected DMA stall.

New regressions sweep empty/full buffers, both enable alignments,
and output boundaries before, on, and after gate expiry. They also
check re-enabling an active sample and restoring a drained buffer
while its refill is waiting for the gate. Existing v4 snapshot fields
already preserve the necessary state, so the format remains v4.

## Results

AccuracyCoin is now **133 pass / 14 fail / 0 skip / 0 untouched**.
The only changed result from the implicit-abort baseline is Delta
Modulation Channel: FAIL 15 -> PASS. Both DMA abort tests retain their
previous passing behavior; no later-revision duplication bug has been
added. See [the full report](results-2026-11-09-dmc-enable.txt).

Validation: all 1,382 library tests, four save-state integration tests,
11 Blargg APU integration tests, five strict DMC/read integration tests,
and the 44-ROM CPU/APU/reset/sprite-DMA/PPU-NMI/MMC3 sweep pass.
