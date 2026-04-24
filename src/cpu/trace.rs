// SPDX-License-Identifier: GPL-3.0-or-later
//! Per-instruction trace gated by env vars.
//!
//! Diffable against Mesen2's [`tools/mesen_trace.lua`] output so the first
//! point where our model diverges from Mesen can be found by line diff.
//! Zero cost when `VIBENES_TRACE_LIMIT` isn't set.
//!
//! Env vars (read once at startup):
//! - `VIBENES_TRACE_LIMIT` — cap on CPU cycles. Beyond this the trace stops
//!   emitting (but emulation continues). Setting any value enables tracing.
//! - `VIBENES_TRACE_START` — optional lower bound; no lines emitted below
//!   this CPU cycle. Default 0.
//!
//! Line format (matches [`tools/mesen_trace.lua`] field-for-field):
//! ```text
//! [M] cyc=N pc=XXXX op=XX a=XX x=XX y=XX sp=XX ps=XX mclk=N
//!     dbr=N dtim=N dbit=N dbuf=0|1 tsd=N ntr=0|1
//! ```

use std::io::Write;
use std::sync::OnceLock;

use crate::bus::Bus;
use crate::cpu::Cpu;

struct TraceConfig {
    limit: u64,
    start: u64,
}

static CONFIG: OnceLock<Option<TraceConfig>> = OnceLock::new();

fn config() -> Option<&'static TraceConfig> {
    CONFIG
        .get_or_init(|| {
            let limit = std::env::var("VIBENES_TRACE_LIMIT").ok()?.parse().ok()?;
            let start = std::env::var("VIBENES_TRACE_START")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            Some(TraceConfig { limit, start })
        })
        .as_ref()
}

pub fn emit_instruction(cpu: &Cpu, bus: &Bus) {
    let Some(cfg) = config() else {
        return;
    };
    let cyc = bus.clock.cpu_cycles();
    if cyc < cfg.start || cyc > cfg.limit {
        return;
    }
    let pc = cpu.pc;
    let op = bus.peek(pc);
    let dmc = bus.apu.dmc_trace();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = writeln!(
        out,
        "[M] cyc={cyc} pc={pc:04X} op={op:02X} a={a:02X} x={x:02X} y={y:02X} sp={sp:02X} ps={ps:02X} \
         mclk={mclk} dbr={dbr} dtim={dtim} dbit={dbit} dbuf={dbuf} tsd={tsd} ntr={ntr}",
        a = cpu.a,
        x = cpu.x,
        y = cpu.y,
        sp = cpu.sp,
        ps = cpu.p.to_u8(),
        mclk = bus.clock.master_cycles(),
        dbr = dmc.bytes_remaining,
        dtim = dmc.timer,
        dbit = dmc.bits_remaining,
        dbuf = if dmc.buffer_empty { 1 } else { 0 },
        tsd = dmc.enable_dma_delay,
        ntr = if dmc.dma_pending { 1 } else { 0 },
    );
}
