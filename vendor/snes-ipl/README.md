# SPC700 IPL ROM

64-byte boot ROM for the SNES audio CPU (SPC700). Mapped at
`$FFC0-$FFFF` while `CONTROL.7 = 1` (the reset state).

- **Origin**: Sony S-SMP IPL routine, original 1990 Nintendo / Sony
  hardware. Sony copyrighted firmware.
- **MD5**: `ac35bfc854818e2f55c2a05917493db3`
- **Size**: 64 bytes
- **Source of this copy**: `~/Git/higan/higan/System/Super Famicom/ipl.rom`
  (byte-identical to Mesen2 `Core/SNES/Spc.h:57-66 _spcBios[64]`,
  bsnes-plus `bsnes/snes/smp/iplrom.cpp`, snes9x `apu/SPC700.h`).

## Why this single file is vendored

The rest of vibenes is clean-room: every CPU/PPU/APU/mapper subsystem
is reimplemented in our own words from public hardware documentation,
with `~/Git/Mesen2`, `~/Git/punes`, `~/Git/nestopia`, and
`~/Git/higan` consulted only for behavioural reference. The SPC700
IPL is the one carve-out, for a specific reason:

1. **The constraint space is fully merged.** 64 bytes, exactly one
   ISA (SPC700), exactly one mailbox protocol (`$AA / $BB / $CC` then
   index/byte upload, fixed by every commercial cart's boot path).
   Many "choices" aren't choices: there is no `MOV SP, #imm` so the
   SP setup must be `MOV X, #$EF; MOV SP, X`; the protocol values
   `$AA`, `$BB`, `$CC` are part of the contract; the reset vector at
   `$FFFE-$FFFF` must point back into the image. A behavioural
   reimplementation done from public docs converges on Sony's bytes
   at most offsets by force of constraint, not by copying. Under
   the merger doctrine that thins the copyright argument either way,
   but it also undermines the *clean-room thesis* the rest of
   vibenes earns - the result wouldn't be credibly independent.

2. **Universal vendoring precedent.** higan, bsnes, Mesen2, snes9x
   all ship the same 64 bytes verbatim. That's not legal cover - it
   is acknowledgement that there is no second IPL to choose from.

3. **Functional irreplaceability.** Commercial carts fingerprint the
   IPL bytes (some copy-protection schemes read `$FFC0-$FFFF` directly).
   A behavioural-only reimplementation would observably diverge from
   real hardware on the long tail. We chose accuracy.

## Override at runtime

Users who would rather supply their own IPL dump can override our
bytes at runtime - same tier ladder as the FDS BIOS:

1. `VIBENES_SPC_IPL` env var (absolute path)
2. `--spc-ipl <path>` CLI flag
3. `config.snes.ipl_path` (settings UI)
4. `$XDG_CONFIG_HOME/vibenes/bios/spc-ipl.rom`

Without an override, the embedded blob is used. See
[`src/snes/smp/ipl.rs`] for the resolver.

## Update procedure

The blob is frozen Sony firmware - it does not change. If a different
hardware revision ever surfaces with a different IPL, bump the MD5
and the cross-check list above.
