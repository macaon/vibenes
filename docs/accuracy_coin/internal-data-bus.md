# Internal and external data bus isolation (2026-11-09)

## Hardware evidence

[NESdev APU status](https://www.nesdev.org/wiki/APU_registers#Master_Control_/_Status_($4015))
describes `$4015` as internal to the CPU: reading it disconnects the
external data bus, so its driven status bits are not visible on the
cartridge data pins. Bit 5 retains the internal bus state.

[Hardware research by Fiskbit](https://www.nesdev.org/wiki/User%3AFiskbit)
describes the split DMA register decoder and internal bus isolation.
The [DMA research discussion](https://forums.nesdev.org/viewtopic.php?t=23543)
explicitly notes that the floating `$4015` bit reflects bus state from
before isolation and cannot be changed by an external value driven
during that isolated cycle. This supports the separation independently
of AccuracyCoin's expected results.

The matching [AccuracyCoin source](https://github.com/100thCoin/AccuracyCoin/blob/14b978badcc98920844de046cede5dc4465db2d6/AccuracyCoin.asm)
(`TEST_InternalDataBus`) deliberately collides a CPU `$4015` read with a
DMC fetch whose low address bits also select `$4015`. The fetched byte
has D5 set, but cannot set internal D5 while the buses are isolated.
Subtest 3 checks the reverse direction: status data must not overwrite
external open bus.

## Root cause and change

The emulator used one latch both for external open bus and for the
floating bit of APU status. A DMA fetch changed that latch, incorrectly
leaking external D5 into the CPU's following `$4015` read.

The bus now tracks two values. Ordinary CPU reads/writes and connected
DMA phases update both. Status reads update internal state without
driving the external latch. When a DMC or OAM read activates a status
alias, its status bit 5 comes from the internal value before the
external memory access; the external memory byte remains separate.
OAM puts drive the copied byte to the buses normally.

The ordinary DMA path still updates the internal latch when there is
no status isolation. This is important: simply hiding all DMA data
from the CPU would fix one case while misrepresenting connected phases.
No DMA timings, enable gates, IRQ delays, or CPU revision choices changed.
Existing limits around electrical sample corruption and PAL register
conflicts remain outside this change; passing this test is not a claim
of complete electrical fidelity. No new console captures were made.

## Save states and validation

The internal latch can differ from the external one across instruction
and frame boundaries. It is now serialized separately in save-state
format **v5**; older states are rejected. Battery saves are unchanged.
A behavioral restore regression checks both values remain distinct.

Other new regressions cover ordinary status reads, connected versus
isolated DMC accesses with opposite D5 values, and OAM/idle status reads.
The existing JSR-through-APU regression now seeds the bus with a real
CPU write rather than assigning only the external latch directly.

AccuracyCoin now reports **134 pass / 13 fail / 0 skip / 0 untouched**.
Only Internal Data Bus changes from the enable-gate baseline: FAIL 2
-> PASS. All remaining failures in this report are PPU tests.
See [the full results](results-2026-11-09-internal-bus.txt).

Validation: all 1,386 library tests, four save-state integration tests,
11 Blargg APU integration tests, five strict DMC/read integration tests,
and the 44-ROM CPU/APU/reset/sprite-DMA/PPU-NMI/MMC3 sweep pass.
