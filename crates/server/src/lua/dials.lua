-- Built-in dials: sdc.light_dial and sdc.volume_dial (loaded into every config).
--
-- While a dial is being turned it steps from its own last value rather than from
-- Home Assistant's state, which lags the device (HA only updates once the light
-- or speaker reports back, and polled integrations take seconds). It sends
-- absolute values and shows its value at once; DIAL_SETTLE_MS after the last turn
-- the engine redraws the dial and it shows HA's state again.
local sdc = ...

local SETTLE = sdc.DIAL_SETTLE_MS

local targets = {} -- ["<entity>:<mode>"] = { v = value, at = sdc.now() }

local function target(key, current)
  local t = targets[key]
  if t and sdc.now() - t.at < SETTLE then return t.v, true end
  targets[key] = nil
  return current, false
end

local function set_target(key, v) targets[key] = { v = v, at = sdc.now() } end

local function forget(entity)
  for key in pairs(targets) do
    if key:sub(1, #entity + 1) == entity .. ":" then targets[key] = nil end
  end
end

local function clamp(v, lo, hi) return math.max(lo, math.min(hi, v)) end
local function round(x) return math.floor(x + 0.5) end

local function check(opts, fn)
  if type(opts) ~= "table" or type(opts.entity) ~= "string" then
    error(fn .. ": { entity = \"...\" } is required", 3)
  end
end

local COLOUR_MODES = { hs = true, xy = true, rgb = true, rgbw = true, rgbww = true }

-- The modes a light supports, in press order.
local function light_modes(a)
  local modes, temp, colour = { "brightness" }, false, false
  for _, m in ipairs(a.supported_color_modes or {}) do
    if m == "color_temp" then temp = true elseif COLOUR_MODES[m] then colour = true end
  end
  if temp then table.insert(modes, "warmth") end
  if colour then table.insert(modes, "hue") end
  return modes
end

local TITLE = { brightness = "", warmth = " · warmth", hue = " · hue" }

--- sdc.light_dial { entity, label?, icon? }
--- Turn: change the current mode. Press: next mode (brightness, then warmth and
--- hue where the light supports them). Tap the touch strip: toggle the light.
function sdc.light_dial(opts)
  check(opts, "sdc.light_dial")
  local entity, label, icon = opts.entity, opts.label or opts.entity, opts.icon
  local slot_mode = {}

  local function state()
    local s = ha.state(entity)
    return s, (s and s.state == "on"), (s and s.attributes) or {}
  end
  local function mode(ctx, a)
    local modes = light_modes(a)
    for _, m in ipairs(modes) do
      if m == slot_mode[ctx.slot] then return m, modes end
    end
    return modes[1], modes
  end
  local function range(a) return a.min_color_temp_kelvin or 2000, a.max_color_temp_kelvin or 6500 end
  local function hs(a) return a.color_mode ~= "color_temp" and a.hs_color or nil end

  return sdc.dial {
    label = label,
    render = function(ctx)
      local _, on, a = state()
      local m = mode(ctx, a)
      local value, indicator
      if m == "warmth" then
        local lo, hi = range(a)
        local k, fresh = target(entity .. ":warmth", on and a.color_temp_kelvin)
        if (on or fresh) and k then
          value, indicator = round(k) .. "K", round((k - lo) / (hi - lo) * 100)
        end
      elseif m == "hue" then
        local c = hs(a)
        local h, fresh = target(entity .. ":hue", on and c and c[1])
        if (on or fresh) and h then
          value, indicator = round(h) .. "°", round(h / 360 * 100)
        elseif on then
          value, indicator = "white", 0
        end
      else
        local p, fresh = target(entity .. ":brightness", on and a.brightness and round(a.brightness / 2.55) or 0)
        if on or (fresh and p > 0) then value, indicator = p .. "%", p end
      end
      return { title = label .. (TITLE[m] or (" · " .. m)), value = value or "off", indicator = indicator or 0, icon = icon }
    end,
    rotate = function(ctx, ticks)
      local _, on, a = state()
      local m = mode(ctx, a)
      if m == "warmth" then
        local lo, hi = range(a)
        local k = target(entity .. ":warmth", (on and a.color_temp_kelvin) or round((lo + hi) / 2))
        k = clamp(k + ticks * 100, lo, hi)
        set_target(entity .. ":warmth", k)
        ha.call("light", "turn_on", { entity_id = entity, color_temp_kelvin = k })
      elseif m == "hue" then
        local c = (on and hs(a)) or { 0, 0 }
        local h = (target(entity .. ":hue", c[1]) + ticks * 10) % 360
        -- from white, jump to a saturated colour so the turn shows
        local sat = c[2] >= 30 and c[2] or 80
        set_target(entity .. ":hue", h)
        ha.call("light", "turn_on", { entity_id = entity, hs_color = { h, sat } })
      else
        local p = target(entity .. ":brightness", on and a.brightness and round(a.brightness / 2.55) or 0)
        p = clamp(p + ticks * 5, 0, 100)
        set_target(entity .. ":brightness", p)
        if p == 0 then
          ha.call("light", "turn_off", { entity_id = entity })
        else
          ha.call("light", "turn_on", { entity_id = entity, brightness_pct = p })
        end
      end
    end,
    press = function(ctx)
      local _, _, a = state()
      local m, modes = mode(ctx, a)
      for i, x in ipairs(modes) do
        if x == m then slot_mode[ctx.slot] = modes[i % #modes + 1] end
      end
    end,
    touch = function()
      forget(entity)
      ha.call("light", "toggle", { entity_id = entity })
    end,
  }
end

--- sdc.volume_dial { entity, label?, icon? }
--- Turn: volume (2% per detent). Press: mute/unmute.
function sdc.volume_dial(opts)
  check(opts, "sdc.volume_dial")
  local entity, label, icon = opts.entity, opts.label or opts.entity, opts.icon
  local key = entity .. ":volume"

  local function attrs()
    local s = ha.state(entity)
    return (s and s.attributes) or {}
  end
  local function current(a) return a.volume_level and round(a.volume_level * 100) or 0 end

  return sdc.dial {
    label = label,
    render = function()
      local a = attrs()
      local v, fresh = target(key, current(a))
      local muted = a.is_volume_muted and not fresh
      return { title = label, value = muted and "mute" or (v .. "%"), indicator = muted and 0 or v, icon = icon }
    end,
    rotate = function(_, ticks)
      local v = clamp(target(key, current(attrs())) + ticks * 2, 0, 100)
      set_target(key, v)
      ha.call("media_player", "volume_set", { entity_id = entity, volume_level = v / 100 })
    end,
    press = function()
      ha.call("media_player", "volume_mute", { entity_id = entity, is_volume_muted = not attrs().is_volume_muted })
    end,
  }
end
