-- Example Stream Deck config (3 rows x 5 cols; key 0 is top-left, row-major).
-- Hot-reloaded on save. Icon paths are relative to this file; an `icon` or full-bleed `image`
-- may also be an http(s) URL or an object key in the S3 assets bucket (see crates/server/README.md).
local sdc = require("sdc")

local ON, OFF = "#f5b400", "#2a2a2e"

-- A toggle button for any light: shows on/off colour and brightness %.
local function light(entity, name)
  return sdc.button {
    render = function()
      local l = ha.state(entity) -- tracked: re-renders only when this entity changes
      local on = l and l.state == "on"
      local label = name
      if on and l.attributes.brightness then
        label = string.format("%s\n%d%%", name, math.floor(l.attributes.brightness / 255 * 100 + 0.5))
      end
      return {
        text = label,
        icon = "icons/bulb.png",
        bg = on and ON or OFF,
        fg = on and "#111111" or "#ffffff",
      }
    end,
    press = function() ha.call("homeassistant", "toggle", { entity_id = entity }) end,
  }
end

local function scene(entity, name)
  return sdc.button {
    render = { text = name, icon = "icons/scene.png", bg = "#7a2e4d" },
    press = function() ha.call("scene", "turn_on", { entity_id = entity }) end,
  }
end

sdc.page("home", {
  -- row 1
  light("light.office", "Office"),
  light("light.desk_light", "Desk"),
  light("light.elgato_bw26k1a07637", "Keylight"),
  light("switch.office_desk_plug", "Desk Plug"),
  sdc.button {
    render = { text = "Media", icon = "icons/play.png", bg = "#3b2f6b" },
    press = function(ctx) ctx.goto_page("media") end,
  },
  -- row 2
  scene("scene.office_concentrate", "Focus"),
  scene("scene.office_relax", "Relax"),
  scene("scene.office_nightlight", "Night"),
  false,
  false,
  -- row 3
  light("light.living_room", "Living"),
  light("light.bedroom", "Bedroom"),
  light("light.entryway", "Entry"),
  false,
  light("light.house", "House"),
})

sdc.page("media", {
  sdc.button {
    render = { text = "Back", icon = "icons/back.png", bg = "#444444" },
    press = function(ctx) ctx.goto_page("home") end,
  },
  sdc.button {
    render = function()
      local p = ha.state("media_player.living_room_2")
      local playing = p and p.state == "playing"
      return {
        text = playing and "Pause" or "Play",
        bg = playing and "#1f7a4d" or "#2a2a2e",
        icon = "icons/play.png",
      }
    end,
    press = function() ha.call("media_player", "media_play_pause", { entity_id = "media_player.living_room_2" }) end,
  },
  sdc.button {
    render = { text = "Vol -", font_size = 22 },
    press = function() ha.call("media_player", "volume_down", { entity_id = "media_player.living_room_2" }) end,
  },
  sdc.button {
    render = { text = "Vol +", font_size = 22 },
    press = function() ha.call("media_player", "volume_up", { entity_id = "media_player.living_room_2" }) end,
  },
  sdc.button {
    render = function()
      local p = ha.state("media_player.living_room_2")
      local title = p and p.attributes.media_title
      return { text = title or "Nothing playing", fg = title and "#ffffff" or "#888888" }
    end,
  },
  -- row 2: a 2x2 span, list position 6 = key 5, covers keys 5, 6, 10, 11: artwork across four
  -- keys. The other positions it covers (7, 11, 12) must be false.
  sdc.span {
    cols = 2, rows = 2,
    render = function()
      local p = ha.state("media_player.living_room_2")
      local art = p and p.state == "playing" and p.attributes.entity_picture
      return art and { image = art } or { text = "Nothing playing", fg = "#888888" }
    end,
    press = function() ha.call("media_player", "media_play_pause", { entity_id = "media_player.living_room_2" }) end,
  },
  false,
  false,
  false,
  false,
  false,
  false,
})

-- Pages are global; a device picks which ones a deck shows. Clients get the device named by
-- --device / SDC_DEVICE, else the sdc.default_device entry for their type, else "*".
sdc.device("office", {
  pages = { "home", "media" },
  home = "home",
  brightness = 60,
  -- Dim after 2 minutes without a key press, off after 10; the next press only wakes the deck
  -- (on the page it was left on). After an hour without a press the deck goes back to its home page.
  idle = { dim_after = 120, dim_brightness = 10, off_after = 600, home_after = 3600 },
})
sdc.default_device { ["*"] = "office" }
