# OAM DMA register activation (2026-11-09)

## Hardware basis and diagnosis

[NESdev DMA](https://www.nesdev.org/wiki/DMA) describes two relevant rules:
RDY can halt only CPU reads, and 2A03 register selection uses the halted
6502 address's upper bits with the DMA address's low five bits.
[Hardware research by Fiskbit](https://www.nesdev.org/wiki/User%3AFiskbit)
also describes the split decoder, based on hardware observations.

AccuracyCoin `TEST_APURegActivation` case 4 copies source page `$40`
while executing ordinary ROM code. That transfer must not read APU
status or clock controllers because the frozen CPU address is outside
`$4000-$401F`. Cases 5-7 deliberately execute from the register window
and verify the opposite case, including source-address mirrors.

The old emulator started DMA synchronously during the `$4014` write
and supplied a synthetic frozen address of `$4014`. It also decoded OAM
source reads as ordinary CPU reads. Therefore copying page `$40`
always read `$4015`, incorrectly clearing the frame IRQ in case 4.

## Change

Writing `$4014` now queues the page and a halt request. DMA starts on
the next actual CPU read, whose address remains available for decoding.
Consecutive writes can replace the page before the CPU halts, including
the two writes performed by `INC $4014`. No synthetic read address or
saved interrupt-poll snapshot is needed; ordinary CPU read timing now
supplies both naturally.

OAM source reads select external memory from the DMA address. With
NTSC register activation enabled by the frozen CPU address, low address
bits can additionally select status or either controller. Status can
supply the internal OAM byte without driving the external bus; the
following OAM write drives the copied byte. If the frozen CPU address
is outside the register window, source page `$40` is open bus and must
not activate those registers.

DMC cancellation now preserves an OAM halt request that is still queued
behind CPU writes, while avoiding an extra halt after OAM has started.

Queued DMA is already represented in snapshots, so the format remains
v3. PAL retains direct register reads when the CPU register window is
active; the new mirror decoding is limited to NTSC. NTSC decoder
behavior is the validated target here; this does not claim complete
PAL register-conflict behavior or electrical fidelity
for competing cartridge/controller data drivers.

## Results

AccuracyCoin improves from **128 pass / 19 fail** to
**130 pass / 17 fail**, with no skipped or unmeasured tests:

- APU Register Activation: FAIL 4 -> PASS, behavior 2.
- INC $4014: FAIL 3 -> PASS.
- Every other result is unchanged.

See [the complete report](results-2026-11-09-oam-activation.txt).

Validation: all 1,373 library tests, four save-state integration tests,
11 Blargg 2005 APU tests, and five strict DMC/read integration tests pass.
The ROM sweep covers 44 ROMs: five CPU interrupt singles, eight APU
singles, six APU reset ROMs, both sprite/DMC DMA ROMs, the official CPU
instruction aggregate (all 16 subtests), ten PPU VBlank/NMI singles,
and both six-ROM MMC3 suites.

New regressions cover queued DMA and last-write selection, page `$40`
with registers disabled, active low-address mirrors, restore before
the halt, DMC cancellation with pending OAM, and both DMA alignment
lengths measured on the servicing read. A PAL regression preserves
direct status reads while enforcing the CPU register-window gate.
