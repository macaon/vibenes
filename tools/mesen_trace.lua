-- tools/mesen_trace.lua - per-instruction + DMA-event trace for Mesen2.
--
-- Mesen2's Lua sandbox strips the `os` module, so trace bounds are baked in
-- by `tools/trace_mesen.sh` via sed on @@LIMIT_CYCLES@@ / @@START_CYCLES@@.
-- Don't edit those tokens manually - use the wrapper.
--
-- Usage (direct):
--   mesen --testRunner tools/mesen_trace.lua <rom> --enableStdout \
--         --doNotSaveSettings --preferences.disableOsd=true
--
-- Output per executed instruction ([M] lines), plus [RDxxxx]/[WRxxxx] for
-- register accesses relevant to DMA timing.
local LIMIT_CYCLES = @@LIMIT_CYCLES@@
local START_CYCLES = @@START_CYCLES@@
local stopping = false

local function b01(v) if v then return 1 else return 0 end end

local function onExec()
  if stopping then return end
  local s = emu.getState()
  local cyc = s["cpu.cycleCount"]
  if cyc > LIMIT_CYCLES then
    stopping = true
    emu.stop(0)
    return
  end
  if cyc < START_CYCLES then return end
  local pc = s["cpu.pc"]
  local op = emu.read(pc, emu.memType.nesMemory, false)
  emu.displayMessage("M", string.format(
    "cyc=%d pc=%04X op=%02X a=%02X x=%02X y=%02X sp=%02X ps=%02X mclk=%d dbr=%d dtim=%d dbit=%d dbuf=%d tsd=%d ntr=%d",
    cyc, pc, op,
    s["cpu.a"], s["cpu.x"], s["cpu.y"], s["cpu.sp"], s["cpu.ps"],
    s["masterClock"],
    s["apu.dmc.bytesRemaining"],
    s["apu.dmc.timer.timer"],
    s["apu.dmc.bitsRemaining"],
    b01(s["apu.dmc.bufferEmpty"]),
    s["apu.dmc.transferStartDelay"],
    b01(s["apu.dmc.needToRun"])))
end

local function onMemEvent(tag)
  return function(address, value)
    if stopping then return end
    local s = emu.getState()
    local cyc = s["cpu.cycleCount"]
    if cyc < START_CYCLES or cyc > LIMIT_CYCLES then return end
    emu.displayMessage(tag, string.format(
      "cyc=%d pc=%04X val=%02X", cyc, s["cpu.pc"], value))
  end
end

emu.addMemoryCallback(onExec, emu.callbackType.exec, 0x0000, 0xFFFF)
emu.addMemoryCallback(onMemEvent("RD4015"), emu.callbackType.read, 0x4015, 0x4015)
emu.addMemoryCallback(onMemEvent("RD4016"), emu.callbackType.read, 0x4016, 0x4016)
emu.addMemoryCallback(onMemEvent("RD4017"), emu.callbackType.read, 0x4017, 0x4017)
emu.addMemoryCallback(onMemEvent("RD2007"), emu.callbackType.read, 0x2007, 0x2007)
emu.addMemoryCallback(onMemEvent("WR4015"), emu.callbackType.write, 0x4015, 0x4015)
emu.addMemoryCallback(onMemEvent("WR4010"), emu.callbackType.write, 0x4010, 0x4010)
emu.addMemoryCallback(onMemEvent("WR4014"), emu.callbackType.write, 0x4014, 0x4014)
