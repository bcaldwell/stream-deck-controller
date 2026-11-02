-- Config for scripts/smoke.mjs: one export of each kind (and a button that also sits in a page).
local lamp = sdc.button {
  label = "Lamp",
  render = function(ctx)
    return { text = "Lamp\n" .. os.date("%H:%M:%S"), bg = "#a87308" }
  end,
  press = function(ctx) print("press from slot " .. ctx.slot) end,
}
sdc.page("home", { lamp })
sdc.export("lamp", lamp)

sdc.export("dimmer", sdc.dial {
  label = "Dimmer",
  render = function(ctx)
    return { title = "Dimmer", value = "42%", indicator = 42, icon = "icons/bulb.png", bg = "#222222" }
  end,
  rotate = function(ctx, ticks) print("rotate " .. ticks .. " from slot " .. ctx.slot) end,
  press = function(ctx) print("dial press from slot " .. ctx.slot) end,
})
