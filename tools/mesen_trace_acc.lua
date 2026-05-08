-- tools/mesen_trace_acc.lua - per-instruction trace for Mesen2,
-- with auto-Start so AccuracyCoin's main menu enters
-- "AutomaticallyRunEveryTestInROM" mode (menuCursorYPos = $FF at boot,
-- selecting which is what Start at the top-of-menu cursor does).
--
-- Mesen2's Lua sandbox strips the `os` module, so trace bounds are
-- baked in by `tools/trace_mesen_acc.sh` via sed on @@LIMIT_CYCLES@@,
-- @@START_CYCLES@@, @@BOOT_FRAMES@@, @@HOLD_FRAMES@@.
--
-- Usage (via wrapper):
--   tools/trace_mesen_acc.sh <rom> <limit_cycles> <start_cycles>
--
-- Output per executed instruction ([M] lines) matching the format that
-- src/nes/cpu/trace.rs emits, so a literal `diff` of the two traces
-- localises the first divergent cycle.

local LIMIT_CYCLES = @@LIMIT_CYCLES@@
local START_CYCLES = @@START_CYCLES@@
local BOOT_FRAMES = @@BOOT_FRAMES@@
local HOLD_FRAMES = @@HOLD_FRAMES@@

local frame = 0
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
  -- Format must match src/nes/cpu/trace.rs::emit_instruction so a
  -- literal diff against vibenes' trace lines up. Field names + order
  -- are identical; values come from Mesen's getState bag.
  print(string.format(
    "[M] cyc=%d pc=%04X op=%02X a=%02X x=%02X y=%02X sp=%02X ps=%02X mclk=%d dbr=%d dtim=%d dbit=%d dbuf=%d tsd=%d ntr=%d",
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

-- Frame counter, advanced by `startFrame`; controller injection
-- happens on `inputPolled` so the values land at exactly the moment
-- the running game samples $4016 (Mesen2's `setInput` from
-- `startFrame` doesn't propagate down to the controller port reads
-- in testRunner mode - the input layer needs to be poked from
-- inside an `inputPolled` callback).
local function onStartFrame()
  frame = frame + 1
end

local function onInputPolled()
  local pressed = (frame >= BOOT_FRAMES) and (frame < BOOT_FRAMES + HOLD_FRAMES)
  emu.setInput({
    a = false, b = false,
    select = false, start = pressed,
    up = false, down = false, left = false, right = false,
  }, 0)
end

emu.addEventCallback(onStartFrame, emu.eventType.startFrame)
emu.addEventCallback(onInputPolled, emu.eventType.inputPolled)
emu.addMemoryCallback(onExec, emu.callbackType.exec, 0x0000, 0xFFFF)
