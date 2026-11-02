-- Demo config used for the screenshots in docs/. Entity names are generic placeholders.
-- Paths are relative to this file.
local sdc = require("sdc")

sdc.brightness(60)
sdc.key_gap(18) -- continue pictures across the bezel between keys
sdc.idle { dim_after = 120, dim_brightness = 10, off_after = 600, home_after = 3600 }

local ON, OFF = "#f5b400", "#2a2a2e"

local function light(entity, name)
  return sdc.button {
    render = function()
      local l = ha.state(entity)
      local on = l and l.state == "on"
      local label = name
      if on and l.attributes.brightness then
        label = string.format("%s\n%d%%", name, math.floor(l.attributes.brightness / 255 * 100 + 0.5))
      end
      return { text = label, icon = "../../icons/bulb.png", bg = on and ON or OFF, fg = on and "#111111" or "#ffffff" }
    end,
    press = function() ha.call("homeassistant", "toggle", { entity_id = entity }) end,
  }
end

local function scene(entity, name, icon)
  return sdc.button {
    render = { text = name, icon = icon or "../../icons/scene.png", bg = "#7a2e4d" },
    press = function() ha.call("scene", "turn_on", { entity_id = entity }) end,
  }
end

local PLAYER = "media_player.living_room"

sdc.page("home", {
  light("light.office", "Office"),
  light("light.desk", "Desk"),
  light("light.keylight", "Keylight"),
  light("switch.desk_plug", "Desk Plug"),
  sdc.button {
    render = { text = "Media", icon = "../../icons/play.png", bg = "#3b2f6b", chevron = true },
    press = function(ctx) ctx.goto_page("media") end,
  },

  scene("scene.focus", "Focus"),
  scene("scene.relax", "Relax"),
  scene("scene.night", "Night", "../../icons/goodnight.png"),
  false,
  false,

  light("light.living_room", "Living"),
  light("light.bedroom", "Bedroom"),
  light("light.entryway", "Entry"),
  false,
  light("light.house", "House"),
})

sdc.page("media", {
  sdc.button {
    render = { text = "Back", icon = "../../icons/back.png", bg = "#444444", chevron = true },
    press = function(ctx) ctx.goto_page("home") end,
  },
  sdc.button {
    render = function()
      local p = ha.state(PLAYER)
      local playing = p and p.state == "playing"
      return { text = playing and "Pause" or "Play", bg = playing and "#1f7a4d" or "#2a2a2e", icon = "../../icons/play.png" }
    end,
    press = function() ha.call("media_player", "media_play_pause", { entity_id = PLAYER }) end,
  },
  sdc.button {
    render = { text = "Vol -", font_size = 22 },
    press = function() ha.call("media_player", "volume_down", { entity_id = PLAYER }) end,
  },
  sdc.button {
    render = { text = "Vol +", font_size = 22 },
    press = function() ha.call("media_player", "volume_up", { entity_id = PLAYER }) end,
  },
  sdc.button {
    render = function()
      local p = ha.state(PLAYER)
      local title = p and p.attributes.media_title
      return { text = title or "Nothing playing", fg = title and "#ffffff" or "#888888" }
    end,
  },
  -- 2x2 span at list position 6 (keys 5, 6, 10, 11). A real setup would use
  -- `p.attributes.entity_picture` here; the demo uses a local picture.
  sdc.span {
    cols = 2, rows = 2,
    render = function()
      local p = ha.state(PLAYER)
      if p and p.state == "playing" then return { image = "art.png" } end
      return { text = "Nothing playing", fg = "#888888" }
    end,
    press = function() ha.call("media_player", "media_play_pause", { entity_id = PLAYER }) end,
  },
  false, false, false, false, false, false,
})

sdc.device("living-room", { pages = { "home", "media" }, home = "home" })
sdc.default_page("home")
