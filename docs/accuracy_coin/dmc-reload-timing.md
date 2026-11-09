# DMC reload timing (2026-11-09)

## Evidence and scope

Hardware behavior, rather than a test score, is the compatibility target.
The [NESdev reference guide](https://www.nesdev.org/wiki/NES_reference_guide)
leads to [APU divider behavior](https://www.nesdev.org/wiki/APU_registers)
and [DMC operation](https://www.nesdev.org/wiki/APU_DMC). These describe
rate selection and a continuously running timer: changing a reload value
must not restart a countdown already in progress.

Blargg's [Detailed DMC Operation](https://www.slack.net/~ant/nes-emu/dmc/)
is a model checked against NES hardware. It supports selecting the
period when each bit's delay begins, not restarting the current delay
on every rate write. It does not independently specify the exact
within-CPU-cycle collision addressed here.

The matching [AccuracyCoin source](https://github.com/100thCoin/AccuracyCoin/blob/14b978badcc98920844de046cede5dc4465db2d6/AccuracyCoin.asm)
(`TEST_DMCReloadTiming`) explicitly says its six-entry answer key was
determined on real consoles. Its Visual 2A03 comment concerns the
output-clock-to-DMA timing; it is not a separate measurement of the
rate-write collision. The exact boundary expectation currently relies
on the test author's reported hardware measurements. We have not
independently reproduced those measurements or established behavior
across all CPU revisions or on PAL hardware. Passing this test alone
is not proof of full hardware fidelity.

## Observed mismatch

A temporary diagnostic captured `$0500-$050B` when the DMC Reload
Timing result was written, before later tests could reuse that RAM.
Each pair records the fast-DMA and slow-DMA probe reads:

| Write relative to expiry (CPU cycles) | Console key reported by AccuracyCoin | Before | After |
| --- | --- | --- | --- |
| -3 | 40 00 | 40 00 | 40 00 |
| -2 | 40 00 | 40 00 | 40 00 |
| -1 | 40 00 | 40 00 | 40 00 |
| 0 | 40 00 | 00 40 | 40 00 |
| +1 | 00 40 | 00 40 | 00 40 |
| +2 | 00 40 | 00 40 | 00 40 |

Only the same-cycle write differed. Our bus clocked the DMC timer
before applying the new `$4010` rate, so a coincident expiry selected
the old period.

## Change

For a CPU write to `$4010`, the bus now makes the rate selector available
to that cycle's timer clock. IRQ-enable and loop controls retain their
normal bus-write ordering. Updating the selector does not change the
current countdown or generate an extra output clock. No global APU
phase shift or DMA delay adjustment is involved. No snapshot state was
added, and the save-state format remains v2.

A bus regression sweeps writes from three cycles before expiry to two
cycles after it, in both fast-to-slow and slow-to-fast directions. It
checks countdown preservation, the reload value, and output-bit count.

## Results and validation

AccuracyCoin now reports **126 pass / 21 fail / 0 skip / 0 untouched**.
The only result changed from the IRQ-inhibition baseline is DMC Reload
Timing: FAIL 1 -> PASS. See [the full report](results-2026-11-09-dmc-reload.txt).

- All 1,359 library tests and four save-state integration tests pass.
- All 11 Blargg 2005 APU integration tests and all five strict-pattern
  dmc_dma_during_read4 integration tests pass.
- All five cpu_interrupts_v2 singles, eight apu_test singles, six
  apu_reset ROMs, both sprdma_and_dmc_dma ROMs, and instr_test-v5
  official_only (all 16 subtests) pass.

The ROM-backed integration tests now default to the user's persistent
`~/Git/vibenes-tests/nes-test-roms` collection and accept `NES_TEST_ROMS`
as an override. Test ROMs remain external to this repository.
