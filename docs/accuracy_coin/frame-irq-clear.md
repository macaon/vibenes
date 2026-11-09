# Deferred frame IRQ clearing (2026-11-09)

Reading `$4015` previously cleared the frame IRQ flag immediately.
AccuracyCoin Frame Counter IRQ subtest 7 requires clearing at the next
get-cycle boundary: a read on get must still report the flag on the
following put cycle, whereas a read on put clears before the next get.

The APU now records a pending clear and applies it before sequencer
updates on the next get. A simultaneous sequencer set wins over the
clear. Reset cancels the request, and save states preserve it. The
save-state format is now version 2; version-1 files are rejected.
Battery saves are unaffected.

Sources: [AccuracyCoin](https://github.com/100thCoin/AccuracyCoin/blob/14b978badcc98920844de046cede5dc4465db2d6/AccuracyCoin.asm)
(`TEST_FrameCounterIRQ`, subtests 6 and 7), and
[NESdev APU status](https://www.nesdev.org/wiki/APU_Status).
The bus decides DMA get/put from the completed-cycle count before
starting a cycle. An even completed count starts get; the APU then
increments its cycle count to odd before the bus access.

## Results

The matching ROM now reports **124 pass / 23 fail / 0 skip / 0 untouched**.
The only changed results from the controller-fix baseline are:

- Interrupt Flag Latency: FAIL E -> PASS.
- DMA + $2002 Read: FAIL 2 -> PASS, behavior 1.
- Frame Counter IRQ: FAIL 7 -> FAIL 13 (hexadecimal).

Frame Counter IRQ now reaches subtest J (encoded error $13). It checks
whether the status flag is temporarily visible during the frame
sequencer IRQ window even with IRQ inhibition enabled. The current
frame-counter implementation suppresses all flag-setting events while
inhibited. This is a separate remaining issue; the deferred clear does
not fix the entire Frame Counter IRQ test.

See [the full results](results-2026-11-09-frame-irq.txt).

## Validation

- 1,355 library tests and four save-state integration tests pass.
- All five cpu_interrupts_v2 singles, eight apu_test singles, six
  apu_reset ROMs, both sprdma_and_dmc_dma ROMs, and instr_test-v5
  official_only (all 16 subtests) pass.
- All 11 blargg_apu_2005.07.30 ROMs pass.
- New tests cover consecutive CPU status reads on both alignments,
  simultaneous set/clear, pending-clear restore, and reset cancellation.
