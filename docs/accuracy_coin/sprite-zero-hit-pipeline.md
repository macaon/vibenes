# Sprite-zero-hit pipeline (2026-11-09)

## Hardware basis

[NESdev PPU rendering](https://www.nesdev.org/wiki/PPU_rendering)
places the earliest sprite-zero hit at dot 2, with pixel output delayed
further to dot 4. [The register reference](https://www.nesdev.org/wiki/PPU_programmer_reference)
describes collision predicates, the exclusion of x=255, and clearing at
pre-render dot 1. These are distinct from sprite-overflow evaluation.

Our renderer evaluates pixel x at dot x+1. It previously set the hit
flag immediately, making the earliest hit visible at dot 1. A pending
collision now reaches the flag on the next dot, at x+2. Pixel output,
background fetches, sprite evaluation, overflow generation, and the
pre-render clear have not moved. This is a correction supported by the
rendering reference independently of AccuracyCoin; no hardware captures
were made in this investigation.

## State and regressions

Pending collision state survives capture/restore and propagates even
when rendering is disabled after detection. Reset discards it, and the
pre-render clear takes precedence. The pending bit is serialized in
save-state format **v6**; older states are rejected. Battery saves are
unchanged.

New regressions exercise x=0, 8, 128, 254, and the excluded x=255;
restore between detection and visibility; disabling rendering after
detection; reset; and pre-render clearing.

## AccuracyCoin and remaining discrepancy

The flag-setting samples at `$51..$57` change from
`00 00 40 60 60 60 60` to `00 00 00 60 60 60 60`. This fixes the early
hit at `$53`; `$54` still contains overflow when the test expects `00`
or `40`. `$2002 flag timing` therefore remains **FAIL 2**. The clearing
samples remain `E0 E0 80 00`, and all other scored results are unchanged:
**134 pass / 13 fail**. See [the report](results-2026-11-09-sprite-zero.txt).

Temporary read-position diagnostics showed successive reads with next
PPU dots 128 through 134 on scanline 128. At next-dot 130 both flags
were clear, while at next-dot 131 both were set. Thus sprite-zero hit
and overflow currently become visible together during processing of
dot 130. Determining overflow's precise visibility within that dot is
the next investigation. [The evaluation reference](https://www.nesdev.org/wiki/PPU_sprite_evaluation)
documents alternating primary reads and secondary writes, but does not
by itself establish a full-dot delay in overflow output. No speculative
overflow delay was added just to obtain a passing result. Temporary
diagnostics were removed.

Validation: all 1,389 library tests, four save-state integration tests,
11 Blargg APU integration tests, five strict DMC/read integration tests,
44 CPU/APU/reset/DMA/PPU-NMI/MMC3 ROMs, and all 16 legacy sprite-hit and
sprite-overflow ROMs pass.
