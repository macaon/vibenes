# OAMDATA during rendering (2026-11-09)

## Read path

[NESdev's register reference](https://www.nesdev.org/wiki/PPU_programmer_reference)
describes `$2004` as exposing internal OAM accesses during rendering.
Previously it always read primary OAM at OAMADDR, so AccuracyCoin's
clear-phase read returned primary data rather than `$FF` (FAIL 4).

The [sprite-evaluation reference](https://www.nesdev.org/wiki/PPU_sprite_evaluation)
documents the underlying sequence: clear returns `$FF`; evaluation
alternates primary reads and secondary accesses; sprite fetch reads
Y, tile, attributes, then X repeatedly; sprite idle reads secondary
byte 0. The implementation now exposes that bus latch. Missing primary
attribute bits are masked when primary OAM is read, rather than masking
every `$2004` result: clear and secondary OAM contain full-width bytes.
Blanking reads retain ordinary primary-OAM behavior.

OAMDATA is live during a CPU read. The bus now samples it after the
existing post-access PPU ticks, including mirrored registers, dummy
reads, and OAM DMA reads. The final byte refreshes both CPU bus latches
and the PPU I/O latch. VBlank's early buffered behavior remains separate.
The matching [AccuracyCoin source](https://github.com/100thCoin/AccuracyCoin/blob/14b978badcc98920844de046cede5dc4465db2d6/AccuracyCoin.asm)
explains this distinction in its timed `$2004` tests. The read-latch
approach also agrees with the downloaded Mesen2 and puNES sources.

## Write path and independent check

The register reference also documents suppressed OAM writes during
rendering and an abnormal address increment. Blanking writes still
store their byte and increment by one. Rendering writes now skip the
RAM store and normally increment by four, keeping the evaluation cursor
consistent with OAMADDR.

The low address bits need care. The final AccuracyCoin subtest expects
OAMADDR 1 to become 4, whereas the simple add-four approximation in the
downloaded Mesen2 source preserves the low bits. We checked this
independently using the public
[Visual 2C02 netlist](https://www.qmtpro.com/~nes/chipimages/visual2c02/).

[The probe](probes/visual2c02-oamdata-write.cjs) starts from the supplied
even pre-render state, enables background rendering through register
pins, and clocks the netlist. During sprite idle it writes OAMADDR at
dot 321 and OAMDATA at dot 324, using 15-master-half-cycle M2-high pulses
with a full 24-half-cycle CPU period. A clear-phase fixture starts on
visible line 0 at dot 20, with OAMDATA written at dot 23. The resulting
address is sampled after settling:

| Phase | Initial OAMADDR | Final OAMADDR |
| --- | --- | --- |
| Sprite idle | 1 | 4 |
| Sprite idle | 3 | 4 |
| Sprite idle | 5 | 8 |
| Secondary clear | 1 | 4 |

[The trace](probes/visual2c02-oamdata-write.txt) records input hashes and
observed addresses. To reproduce, use the eight simulator files listed
in the [overflow investigation](sprite-overflow-output-latch.md):

```sh
node docs/accuracy_coin/probes/visual2c02-oamdata-write.cjs /path/to/visual2c02 1
node docs/accuracy_coin/probes/visual2c02-oamdata-write.cjs /path/to/visual2c02 3
node docs/accuracy_coin/probes/visual2c02-oamdata-write.cjs /path/to/visual2c02 5
node docs/accuracy_coin/probes/visual2c02-oamdata-write.cjs /path/to/visual2c02 1 clear
```

This supports `(address + 4) & $FC` for the tested NTSC clear/idle
phases. That low-bit rule is not extended to PAL or active evaluation,
where additional increment conditions need separate investigation.
This probe checks the address, not all write-related electrical or
corruption effects. It is transistor-model evidence, not a new physical
console measurement.

The probe also distinguishes register-access assertion from release.
CPU and OAM-DMA `$2004` writes now apply at access completion after the
existing post-access ticks. A write crossing the end of sprite fetch
therefore acts in its ending phase. No CPU or DMA cycles were added.
Temporary diagnostics confirmed that the ROM's final, longer setup
actually reaches the next line's clear phase despite its reused
pre-render timing comment; the clear-phase probe avoids assuming that
comment is an exact account of the executed timing.

## Validation and limits

All ten Address `$2004` behavior subtests now pass (`$41`, the ROM's
revision-G success code). Misaligned OAM2 Address advances from FAIL 3
to FAIL 4. No other scored results change. AccuracyCoin reports **137
pass / 10 fail / 0 skip / 0 untouched**; see
[the full report](results-2026-11-09-oamdata.txt). `$2004 Stress Test`
still fails subtest 2; this is not complete OAM bus/electrical emulation.

Regressions cover live sampling across clear/evaluation through four
bus paths; primary attribute masking versus full-width secondary bytes;
all eight fetch slots and X repeats; sprite idle; blocked versus normal
writes; address wrap; and a write crossing the fetch boundary.
All 1,398 library tests, 20 integration tests, and 60 regression ROMs
pass. Existing serialized latches suffice; save-state format remains v8.
