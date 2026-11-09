# Implicit DMC DMA abort (2026-11-09)

## Hardware target and evidence

[NESdev DMA bugs](https://www.nesdev.org/wiki/DMA#Bugs) distinguishes
the common one-cycle aborted DMA from a separate unexpected full DMA
found on later RP2A03G and RP2A03H CPUs. The short abort is suppressed
if a CPU write prevents the halt. PAL and clone behavior is not fully
established. These are distinct hardware cases, not interchangeable
answer keys to select for a higher test score.

The existing emulator did not implement the later revision's extra
full DMA. This change leaves that limitation in place and repairs the
common aborted reload. The result matches the older CPU pattern that
AccuracyCoin accepts as behavior 2. There is no new CPU revision toggle,
no change to enable/disable delays, and no injected full fetch solely
to force the later revision's pattern.

The [DMC output unit reference](https://www.nesdev.org/wiki/APU_DMC#Output_unit)
describes consuming the fetched sample buffer at an output-cycle
boundary. The matching [AccuracyCoin source](https://github.com/100thCoin/AccuracyCoin/blob/14b978badcc98920844de046cede5dc4465db2d6/AccuracyCoin.asm)
checks three separate timing tables, including write-suppression and
a looping control case. [Mesen2's DMC implementation](https://github.com/SourMesen/Mesen2/blob/master/Core/NES/APU/DeltaModulationChannel.cpp) was also consulted
as an implementation reference, not as a hardware authority.

## Root cause and correction

The existing one-byte-stop glitch handler cleared the fetched buffer
immediately. The following output boundary therefore saw an empty
buffer, never performed its normal consume/refill transition, and
never requested the short aborted reload.

The fetched byte now remains buffered until that boundary. The old
output cycle finishes normally; the new sample is not prematurely
copied into its shift register or made audible early. Buffer consumption
then requests the reload, and the existing delayed disable cancels it
after one halt cycle. If that cycle is a write instead, cancellation
withdraws the request without stalling a later read.

## Recorded measurements

Before the correction, both non-looping tables were all zero: the
short abort was entirely absent. The looping control table was already
correct. After the fix, the three 16-entry tables match the documented
older-CPU answer key exactly:

```text
$0500: 00 00 00 00 00 00 00 00 00 00 01 01 00 00 00 00
$0520: 00 00 00 00 00 00 00 00 00 00 01 00 00 00 00 00
$0540: 00 00 00 00 00 00 00 00 00 00 04 04 04 04 04 04
```

The missing second pulse in `$0520` is the write-suppression case.
The later CPU pattern would additionally show `04` at indices 8 and 9;
that sample-duplication behavior remains unimplemented. No new physical
console measurements were made here.

## Results and validation

AccuracyCoin reports **132 pass / 15 fail / 0 skip / 0 untouched**.
Only Implicit DMA Abort changes from the explicit-abort baseline:
FAIL 2 -> PASS, behavior 2. See [the full report](results-2026-11-09-implicit-abort.txt).

New unit tests verify buffer retention, finishing the old output bit,
refill scheduling and cancellation, exclusions for looping/multibyte
samples and other timing windows, plus one-cycle stalling versus
write-suppression through the bus. Existing v4 snapshot fields already
preserve this state; no additional format change is needed.

Validation: all 1,379 library tests, four save-state integration tests,
11 Blargg APU integration tests, five strict DMC/read integration tests,
and the 44-ROM CPU/APU/reset/sprite-DMA/PPU-NMI/MMC3 sweep pass.
