# Controller strobe timing (2026-11-09)

The old bus forwarded every `$4016` write directly to both controllers.
A one-CPU-cycle high pulse therefore always loaded their shift registers,
regardless of CPU/APU alignment. AccuracyCoin Controller Strobing failed
subtest 4 because one of the two alignments must ignore that pulse.

## Hardware basis

[NESdev controller registers](https://www.nesdev.org/wiki/Controller_port_registers)
describes the CPU output latch and the controller's separate shift
register. [Standard controller reading](https://www.nesdev.org/wiki/Controller_reading_code)
describes continuously loading buttons while strobe is high.

The independently documented
[EPSM controller strobe detection technique](https://www.nesdev.org/wiki/Expansion_Port_Sound_Module#Controller_strobe_detection)
uses a one-cycle RMW strobe to detect CPU/APU alignment on physical
hardware. Thus alignment-dependent strobes have a practical hardware
basis beyond AccuracyCoin's answer key. That reference does not by
itself identify the exact sampling phase.

The matching [AccuracyCoin assembly](https://github.com/100thCoin/AccuracyCoin/blob/14b978badcc98920844de046cede5dc4465db2d6/AccuracyCoin.asm)
(`TEST_ControllerStrobing`, cases 3 and 4) checks both alignments of
`DEC $4016`. Distinguish the CPU register write from the later output-pin
transition: a value written during get is available when put begins,
before the CPU performs its write in that put cycle. Sampling after
that write incorrectly reverses which one-cycle pulse is recognized.
The new model samples before the put-cycle bus access. It matches both
test alignments and the separate hardware-documented alignment effect;
we have not independently captured the sub-cycle signals on a console.

## Change

The bus now retains the CPU-written D0 separately from the controller
strobe pins. At the start of put it transfers that level to both pads.
A high written on get reaches the pins on put; a put-only high can be
overwritten on the following get before it is sampled. A longer pulse
is recognized from either initial alignment. Upper write bits do not
control the standard controller latch.

The pending register value and effective pin state can differ at a
snapshot boundary. The bus snapshot now saves both, and the format
version is **3**. Earlier save states are rejected; battery-save files
are unaffected.

## Results

AccuracyCoin reports **127 pass / 20 fail / 0 skip / 0 untouched**.
The only changed result from the DMC-reload baseline is Controller
Strobing: FAIL 4 -> PASS. Controller Clocking retains PASS behavior 2.
See [the full report](results-2026-11-09-controller-strobe.txt).

New regressions check one-cycle pulses on both alignments, serial
reports on both ports, a pulse held across the sampling boundary,
ignored upper write bits, and restoration before a pending strobe is
sampled. The existing CPU/APU/DMA and save-state tests remain the gates.
