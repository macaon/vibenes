-- Does os module exist?
emu.displayMessage("T", "os=" .. tostring(os))
if os and os.getenv then
  emu.displayMessage("T", "has getenv; T=" .. tostring(os.getenv("TRACE_CYCLES")))
end
emu.stop(0)
