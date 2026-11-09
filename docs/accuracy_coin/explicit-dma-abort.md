# Explicit DMC DMA abort (2026-11-09)

## Evidence

[NESdev DMA bugs](https://www.nesdev.org/wiki/DMA#Bugs) documents a
single-cycle aborted DMA near sample termination and its suppression
when a write delays the halt. It also distinguishes CPU revisions for
the implicit-stop extra-DMA bug and notes that PAL behavior is not
established. The patch does not change enable/disable delay constants
or choose a different CPU revision to obtain a passing score.

The matching [AccuracyCoin source](https://github.com/100thCoin/AccuracyCoin/blob/14b978badcc98920844de046cede5dc4465db2d6/AccuracyCoin.asm)
(`TEST_ExplicitDMAAbort`) sweeps 16 alignments. Its measurements depend
on DMA phase after re-enabling playback, not just an isolated count of
halted cycles. They must be checked alongside whether the DMC buffer
was filled. This is why simply adding a delay or forcing an extra
sample fetch is insufficient.

[Mesen2's DMA implementation](https://github.com/SourMesen/Mesen2/blob/master/Core/NES/NesCpu.cpp)
was consulted as a behavioral reference for cancellation during the
halt versus later phases. It is an implementation reference, not an
independent physical measurement. The precise 16-entry comparison here
relies on AccuracyCoin's console answer key; no new console captures
were made in this session.

## Observations and change

The recorded 16 measurements before the fix were:

```text
04 04 04 04 04 04 02 03 01 01 00 00 00 00 00 00
```

The expected and corrected measurements are:

```text
04 04 04 04 04 04 03 04 01 01 00 00 00 00 00 00
```

Only offsets 6 and 7 differed. A cancellation detected after the halt
exited the DMA loop without clocking the following idle phase. The
bus now distinguishes:

- Cancellation during halt: release after the one halt cycle.
- Cancellation latched after halt: finish an idle read phase before
  releasing, without fetching a new sample. If OAM is active, its
  next transfer supplies the phase instead of adding an idle cycle.
- Cancellation arriving during an already-started sample fetch: finish
  the fetch and clear the abort latch so it cannot affect a later DMA.

No enable-delay, disable-delay, period, or parity constants changed.
The tests check both alignments, the byte-buffer result, write-delayed
cancellation, and post-restore timing.

## Save states

The existing DMC `disable_delay` and `abort_pending` fields, and the
bus `dmc_abort` latch, were absent from the snapshot schema. All three
are now captured and restored. Behavioral restore tests independently
exercise the countdown, pending APU event, and bus latch. The format
is **v4**, so older save-state files are rejected. Battery saves are
unaffected.

## Results and limits

AccuracyCoin improves to **131 pass / 16 fail / 0 skip / 0 untouched**.
Only Explicit DMA Abort changes from the OAM-activation baseline:
FAIL 2 -> PASS. Implicit DMA Abort remains FAIL 2 and is a separate
remaining target. See [the complete results](results-2026-11-09-explicit-abort.txt).

The unit boundary sweep checks timer-independent cancellation at the
first three DMA clocks on both alignments, including whether a sample
was fetched. This does not establish every revision-specific implicit
abort effect or validate PAL hardware behavior.

Validation: all 1,376 library tests, four save-state integration tests,
11 Blargg APU integration tests, five strict DMC/read integration tests,
and the 44-ROM CPU/APU/reset/sprite-DMA/PPU-NMI/MMC3 sweep pass.
