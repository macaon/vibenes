# DMC DMA register activation (2026-11-09)

## Hardware evidence

[NESdev DMA register conflicts](https://www.nesdev.org/wiki/DMA#Register_conflicts)
describes the 2A03 decoder: register activation combines the halted
6502 core's upper address bits with the DMA address's low five bits.
Thus a DMC fetch can activate `$4015`, `$4016`, or `$4017` if the
halted CPU address is in `$4000-$401F`, even though the cartridge sees
a sample address elsewhere. It must not activate them when the CPU
is halted outside that range.

The [2021 hardware research discussion](https://forums.nesdev.org/viewtopic.php?t=23543)
includes console measurements of DMA/controller interactions and
explains the register gating and the possibility of corrupted sample
bytes. [The internal/external bus discussion](https://forums.nesdev.org/viewtopic.php?t=25289)
explains why `$4015` is special: its value never drives the external
bus. These references support investigating the decoder independently
of AccuracyCoin's expected output.

## Change

DMC cartridge reads now also apply the NTSC register decoder:

- A sample address ending in low-five-bit `$15` triggers the APU status
  read side effect without replacing the sample on the external bus.
- Low bits `$16` or `$17` read the selected controller. The CPU-visible
  bus retains sample bits D5-D7 and uses the controller's driven low
  bits, matching the emulator's existing standard-controller wiring.
- Other addresses do not add register side effects.

This is one DMA bus cycle, not two separately clocked CPU reads.
No DMA cycle counts, timer phases, or save-state fields are changed.
The snapshot format remains v3.

## Limits

This change fixes the CPU-visible conflict result, not every electrical
consequence. The DMC sample path still receives the cartridge byte;
corruption of that byte by simultaneously driven controller lines is
not implemented. AccuracyCoin explicitly says this test cannot measure
the byte received by the DMC, so passing it does not validate that path.

PAL behavior is different and not fully understood in the DMA reference.
The new NTSC conflict behavior is not applied to PAL. This preserves the
existing PAL behavior rather than treating an NTSC answer key as a PAL
hardware specification.

This does not generalize register activation to OAM DMA or change the
existing contiguous controller-read clock model. Those are separate
accuracy issues; APU Register Activation still fails its existing case 4.

## Results

AccuracyCoin is now **128 pass / 19 fail / 0 skip / 0 untouched**.
Only DMC DMA Bus Conflicts changed from the controller-strobe baseline:
FAIL 2 -> PASS. See [the full report](results-2026-11-09-dmc-conflicts.txt).

New tests exercise the decoder window boundaries, low-five-bit mirrors,
both ports with zero and one sample bits, status-read side effects and
external-bus isolation, an actual DMC fetch through the DMA loop, and
the PAL exclusion.

Validation: all 1,367 library tests, four save-state integration tests,
11 Blargg 2005 APU tests, five strict-pattern DMC/read tests, and the
22 CPU/APU/reset/sprite-DMA regression ROMs pass (including all 16
subtests of instr_test-v5 official_only).
