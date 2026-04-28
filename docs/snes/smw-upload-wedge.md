# SMW SPC IPL Upload Wedge — left for later

State on pause: `master` at commit (after Phase 6.1 per-access scheduler).
Suite: 933/933 lib + all integration tests green.

## What works

- `Snes::from_cartridge` parses SMW (USA), reset vector resolves to `$00:8000`.
- CPU executes far enough into SMW's reset code to reach the SPC IPL block-upload loop at `$00:8095..$00:80A8`.
- SMP IPL boot signature (`$AA`/`$BB`) is correctly placed in `smp_to_cpu`.
- The first ~241 bytes of SMW's first audio-driver block upload **do** transfer correctly: SMP echoes counters `$00..$F1` in lockstep with the CPU's writes.

## Where it wedges

After the CPU writes counter `$F2` (242nd byte), the SMP **escapes the IPL ROM** and begins wandering ARAM. From then on `smp_pc` walks linearly through ARAM (advancing ~8500 per ~17000 SMP cycles → roughly one byte per 2 SMP cycles → most of ARAM is decoded as junk SPC700 instructions, mostly NOPs and short ops).

Steady-state mailbox view (frozen forever once SMP escapes):

```
mb_cpu_view = [F1 03 D5 60]   (smp_to_cpu)
mb_smp_view = [F2 8D 00 00]   (cpu_to_smp)
```

`F1` = SMP's last echo. `F2` = CPU's last counter. `D5 60` in cpu_view positions 2/3 are residue from the wandering SMP randomly executing opcodes that hit `$F6`/`$F7`.

The CPU is stuck at `cpu_pc=$809a/$809d` (the per-byte poll loop) waiting for the SMP to echo `$F2` — but the SMP is no longer in the echo loop.

## What we already tried (none unblocked it)

1. **Phase 6.0** — unified `Snes::master_cycles` and `bus.master` so SMP catch-up reads from the real master clock (commit 98b49a5). Correct fix on its own merits; SMP timing is now accurate by a constant factor.
2. **Removed `pending_cpu_to_smp` dual-latch** (commit 73c1320). Was added historically for Kishin Douji Zenki / Kawasaki Superbike — over-corrective and broke the standard upload protocol. Real hardware exposes mailbox writes immediately. Still didn't help SMW.
3. **Phase 6.1 — per-access SMP scheduler** (current commit). Replaced the batched `run_smp_to_master_cycles` with a `SchedulerCtx` that wraps `&mut LoRomBus + &mut ApuSubsystem`, implements `SnesBus`, and runs the SMP forward to the bus's master clock after every CPU bus access. Reordered ops so SMP catch-up runs *between* the master advance and the data latch (matching hardware: SMP can write `$F4` *during* the CPU's access window, and the CPU sees the post-window value). Architecturally correct; **same wedge**.

## Current best hypotheses for the underlying cause

1. **Counter / Y miscount in our SMP** — some flag-handling or instruction-cycle edge case in the IPL inner loop ($FFDA-$FFE9) lets Y wander away from `$F4`'s value. After many bytes the divergence accumulates until `$F4 > Y` (or Y wraps past `$80`), tripping the transfer-end path at `$FFEF` → `JMP [$0000+X]` lands in garbage ARAM.
2. **Master-cycle accounting on SMP side** — the SMP's per-instruction cycle costs may not match what the bsnes IPL ROM expects, causing SMP to advance a different number of bytes per CPU iteration than real hardware.
3. **CPU-side bus-access count off** — if our 65C816 is doing fewer (or more) bus accesses per instruction than real hardware, the SMP gets a different number of catch-up windows per CPU loop iteration. The next thing to investigate is exactly how many `bus.read/write/idle` calls our CPU issues for each instruction in SMW's poll loop, and compare against the 65C816 reference.

## Immediate next steps if/when we resume

1. **Capture an instruction-by-instruction trace** of the SMP from `$FFD6` onwards while SMW's upload is in progress. Compare against bsnes / Mesen2 traces for the same boot. The divergence point (where our SMP takes a different branch than the reference) is the bug.
2. Specifically validate the IPL inner-loop branch flags after every byte. The relevant tests:
   - After `$FFE0 MOV $F4, Y` ; `$FFE2 MOV [$00]+Y, A` ; `$FFE4 INC Y` ; `$FFE5 BNE $FFDA` — does our INC correctly set Z when Y wraps from `$FF` to `$00`? Does BNE correctly read that Z?
   - At `$FFE9 BPL $FFDA` and `$FFED BPL $FFDA`, our N flag handling on `CMP Y, $F4` for high-bit-set values.
3. Add an `ApuSubsystem` debug path that emits one trace line per SMP instruction while in IPL ROM (gated on `VIBENES_SMP_TRACE_IPL`). Compare against a known-good reference dump of the same first-frame execution.
4. Once the SMP-side bug is found, validate against PeterLemon SPC700 ISA tests (already passing — so the bug is something they don't exercise, likely a specific branch-flag combination or a cycle-count mismatch on a specific opcode used by the IPL).

## Diagnostic invocation

```bash
VIBENES_SNES_AUDIO_DEBUG=1 ./target/release/vibenes "<path>/Super Mario World (USA).sfc" 2>&1 | head -25
```

The first 5 frames log every frame; subsequent every 30 frames.
