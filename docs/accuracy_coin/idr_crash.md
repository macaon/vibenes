# AccuracyCoin "Implied Dummy Reads" trap (open)

## Symptom

Running `accuracy_coin` against `~/Git/nes-test-roms/AccuracyCoin.nes`
gets stuck in the test ROM's panic landing pad:

- PC busy-loops at `$80DF` (`JMP $80DF`).
- Result byte for **Implied Dummy Reads** (`$046D`) stays `$00`
  (untouched).
- The next four Page 20 result bytes (`$048B` Branch Dummy Reads,
  `$047C` JSR Edge Cases, `$0490` Internal Data Bus) also never run.

The runner now detects a sustained PC at `$80DF` and aborts the frame
loop early so the report still renders, with a `note: test ROM trapped`
banner. `accuracy_coin` reports 117 / 18 / 5 untouched as of this
writing.

## What we know

PC reaches `$80DF` for the first time at cycle 139 460 936 (frame
4430), and it arrives via **RTI from `$F859`**, not via the
`$808C` fail-handler entry (which our diag harness verified is
**never** entered during the run).

At the moment the RTI fires, the stack has:

```
$1ED = $25  (P)
$1EE = $DF  (PCL)
$1EF = $80  (PCH)
```

so RTI restores `PC = $80DF`.

Searching the PRG ROM for the obvious stack-craft patterns turns up
nothing:

- No `LDA #$80; PHA; LDA #$DF; PHA` (RTI-style craft).
- No `LDA #$80; PHA; LDA #$DE; PHA` (RTS-style craft).
- No `JSR $80E0` (which would push `$80 $DF` as return-1).
- No 16-bit pointer table containing `DF 80`.
- The bytes `DF 80` appear exactly once in PRG, inside the `JMP $80DF`
  trap itself.

The NMI vector is RAM `$0700` which loads `JMP $9246`. The handler at
`$9246` only reaches the fail-handler (`JMP $808C`) when zp `$19` bit 4
is set; in our run that branch is never taken.

So either an earlier path landed at `$80DF` outside our detection
window, or the NMI saved-PC slots `$1EE/$1EF` are populated by a
mechanism we haven't traced yet.

## Diagnostic harness

`src/bin/acc_diag.rs` (deleted from this branch, easy to recreate from
git history if needed) single-stepped the CPU with a 4096-entry ring
buffer and dumped state on first PC=`$80DF`. The relevant findings
above came from that tool. The trace harness at
`tools/{trace_mesen.sh,mesen_trace.lua}` and `src/cpu/trace.rs` is
also available; what's missing is a **Mesen-side run that auto-presses
Start** (Mesen's `--testRunner` doesn't drive the controller). Picking
that up first will let us cycle-diff vibenes against Mesen2 inside the
suspect window.

## Where this lives in the roadmap

P0 in the AccuracyCoin remediation plan. Blocks measurement of the
last four Page 20 tests, but those tests are largely **shared P2 fails
with Mesen2** (Mesen2 also fails Branch Dummy Reads, Implied Dummy
Reads, Internal Data Bus). Real ceiling impact of unblocking is small;
correctness impact on commercial games is unknown but unlikely.

## Next steps when resuming

1. Get a Mesen2 trace of AccuracyCoin from boot through the IDR test.
   The `--testRunner` flag wants a Lua harness that pokes `$4016` to
   simulate Start. See `tools/mesen_trace.lua` for the existing
   instruction-trace template.
2. Diff vibenes' trace from `src/cpu/trace.rs` against Mesen's at the
   first divergent cycle.
3. The likely fault domain is implied / RMW dummy-read addressing or
   the BRK/IRQ stack-frame model, not the SH* family (already fixed).
