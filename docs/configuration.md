# Configuration guide

The whole deck is described by one Lua file, `config.lua`, read by the server (`SDC_CONFIG`, or the first command
line argument). This guide walks through the concepts with examples; the exhaustive tables (every environment
variable, every message of the plugin protocol) are in [crates/server/README.md](../crates/server/README.md).
A complete working file is [config.lua](../config.lua) at the repository root; the one behind the screenshots is
[demo/config.lua](demo/config.lua).

Every file starts with:

```lua
local sdc = require("sdc")
```

`ha` is a global (also `sdc.ha`). Other `.lua` files next to the config can be loaded with `require`.

## Hot reload and checking a layout

The config is reloaded when you save it. A config that fails to load (syntax error, bad span, duplicate export id)
is logged and the previous one stays active, so a typo never blanks the deck. Connected decks are re-rendered
without being woken or dimmed.

To check a layout without hardware, use the `render` subcommand. It writes one PNG per key and a stitched
`page.png`:

```sh
sdc-server render --config config.lua --out /tmp/out --page home --states docs/demo/states.json
```

With `HA_URL`/`HA_TOKEN` set it reads real states; otherwise empty ones, or the fake ones from `--states` (a JSON
list of `{entity_id, state, attributes}`).

## Buttons and render fields

A page is a list of keys, row-major, first key top-left. Use `false` (or `nil`) for an empty key.

```lua
sdc.page("home", {
  sdc.button {
    render = { text = "Media", icon = "icons/play.png", bg = "#3b2f6b" },
    press  = function(ctx) ctx.goto_page("media") end,
  },
  false,                                   -- empty key
})
sdc.default_page("home")                   -- optional; default is the first page defined
```

`render` is a table, a string, or a function returning one. The fields:

| Field | Meaning |
|-------|---------|
| `text` | Label. `\n` allowed; wraps and shrinks to fit |
| `bg`, `fg` | Background and text colour, `#rgb` or `#rrggbb` |
| `icon` | A picture fitted inside the key over `bg`, with padding. With `text`, the icon takes the top part and the label sits along the bottom |
| `tint` | `#rrggbb`: recolours the `icon`'s visible pixels to this colour, keeping their alpha (an icon with no transparency uses its luminance as alpha). Icons only, never `image` |
| `image` | A full-bleed picture, scaled to cover the key (center-cropped). `text` is drawn over a dark gradient at the bottom |
| `font_size` | Pixels at a 72 px key, scaled with the real key size. Auto-fit when absent |

`press` and `release` handlers are optional and receive `ctx`:

| `ctx` field | Meaning |
|-------------|---------|
| `client_id`, `page` | Which deck and which page |
| `key` | 0-based key index |
| `rows`, `cols` | The deck's grid |
| `goto_page(name)` | Switch this deck to another page |
| `span_cols`, `span_rows`, `span_index` | For spans, see [Spans](#spans) |

A Lua error in `render` or `press` is logged and shows a red `ERR` tile on that key; the server keeps running.

## Home Assistant state and services

```lua
local function light(entity, name)
  return sdc.button {
    render = function()
      local l = ha.state(entity)                    -- {entity_id, state, attributes} or nil
      local on = l and l.state == "on"
      local label = name
      if on and l.attributes.brightness then
        label = string.format("%s\n%d%%", name, math.floor(l.attributes.brightness / 255 * 100 + 0.5))
      end
      return { text = label, icon = "icons/bulb.png",
               bg = on and "#f5b400" or "#2a2a2e", fg = on and "#111111" or "#ffffff" }
    end,
    press = function() ha.call("homeassistant", "toggle", { entity_id = entity }) end,
  }
end
```

- `ha.state(entity_id)` returns the entity as a table, or `nil` if it is unknown (or Home Assistant is not
  connected). Always handle `nil`.
- `ha.call(domain, service, data)` calls a Home Assistant service. It is fire-and-forget: there is no return value
  and failures are only logged.
- **Dependency tracking is automatic.** Every entity passed to `ha.state` while a key renders is recorded for that
  key. When Home Assistant reports a change, only the keys that read that entity are re-rendered, and a frame is
  sent only if the image changed. There is nothing to subscribe to and no polling. Read the entities inside the
  `render` function, not at the top of the file, or they will not be tracked.
- Without `HA_URL`/`HA_TOKEN`, or while Home Assistant is down, the server still runs: `ha.state` returns `nil` and
  `ha.call` is dropped. The connection reconnects with backoff and re-reads all states.

## Pages

```lua
sdc.page("media", { ... })
sdc.default_page("home")
```

- Switch pages from a handler with `ctx.goto_page("media")`. Each deck has its own current page.
- The server remembers each client's page by `client_id`, so a deck that reconnects (server redeploy, client
  update) returns to where it was, unless that page no longer exists.
- `sdc.idle { home_after = 3600 }` sends a deck back to the default page after that many seconds without a press
  (see [Idle](#idle-dimming-and-power)).

## Spans

`sdc.span { cols, rows, render, press, release, gap }` is a button covering a rectangle of keys and drawing one
picture or text across all of them. A span in list slot `k` occupies `k .. k+cols-1` on that row and the same
columns on the following rows. The other slots it covers must be `false`/`nil`.

The classic use is album art over 2x2 keys:

```lua
sdc.page("media", {
  sdc.button { render = { text = "Back" }, press = function(ctx) ctx.goto_page("home") end },  -- key 0
  false, false, false, false,                                                                 -- keys 1-4
  sdc.span {                                  -- slot 5: covers keys 5, 6, 10, 11
    cols = 2, rows = 2,
    render = function()
      local p = ha.state("media_player.living_room")
      local art = p and p.state == "playing" and p.attributes.entity_picture
      return art and { image = art } or { text = "Nothing playing", fg = "#888888" }
    end,
    press = function(ctx) ha.call("media_player", "media_play_pause", { entity_id = "media_player.living_room" }) end,
  },
  false,                                      -- key 6 (covered)
  -- keys 7-9 may be buttons; keys 10 and 11 are covered by the span and must stay empty
})
```

Notes:

- `attributes.entity_picture` is a Home Assistant path such as `/api/media_player_proxy/...`. The server fetches it
  with your `HA_TOKEN`; see [Icons and images](#icons-and-images).
- The picture is laid out once on a canvas the size of the whole span and cut into one tile per key. With a gap
  (`gap = 18` on the span, or `sdc.key_gap(18)` for every span) the picture continues across the space between
  keys, as on a real Stream Deck MK.2, at the cost of hiding the strip behind the bezel. The default gap is 0.
- In `press`, `ctx.key` is the pressed key and `ctx.span_index` its position inside the span (0..n-1, row-major).
- A span that runs off the deck or overlaps another button is logged and shown as `ERR` tiles. The grid size is per
  client, so this is checked when each deck renders.

## Icons and images

`icon` and `image` take the same kind of string, resolved in this order:

1. `http://` or `https://` URL, downloaded.
2. A path starting with `/api/`: fetched from Home Assistant (`HA_URL` + path, `Authorization: Bearer <HA_TOKEN>`).
   This is how media artwork works, because `attributes.entity_picture` is such a path and changes with every
   track. Artwork that cannot be fetched (stopped player, 404) is just rendered as if no picture were given.
3. A file relative to the config directory, such as `icons/bulb.png`.
4. An object key in the S3 assets bucket, when the `SDC_ASSETS_S3_*` variables are set (read-only credentials are
   enough; any S3-compatible store works).

```lua
{ text = "Movies", image = "tv/popcorn.png" }                      -- bucket object
{ text = "Lamp", icon = "https://example.com/bulb.png", bg = "#FFA000" }
{ image = "/api/media_player_proxy/media_player.living_room?token=..." }
```

Rendering never waits for the network. Downloads are cached decoded in memory; a key whose picture is not there
yet renders without it and is re-rendered as soon as the download completes. A failed download shows a red `ERR`
in the picture area and is retried after 60 s or on the next config reload. Static `icon`/`image` strings in the
config are prefetched on every load.

## Idle, dimming and power

```lua
sdc.brightness(60)                                    -- active brightness, 0-100
sdc.idle { dim_after = 120, dim_brightness = 10, off_after = 600, home_after = 3600 }   -- seconds
```

- After `dim_after` seconds without a press the deck is dimmed to `dim_brightness` (default 10); after
  `off_after` the backlight goes off (brightness 0). Both optional. Without `sdc.idle` a deck never idles.
- The next press restores the brightness and is consumed: it only wakes the deck, its handlers do not run.
- `home_after` returns the deck to the default page after that many seconds without a press, even while the
  display is off, so waking shows the home page.
- The Pi client turns "backlight off" into a USB suspend, see [Raspberry Pi deck](pi.md#power-saving).

## Exports and dials (Elgato plugin)

`sdc.export(id, item)` makes a button, span or dial visible to the [Elgato plugin](plugin.md) and returns `item`,
so the same button can also be placed in a page:

```lua
sdc.export("living_light", light("light.living_room", "Living"))

sdc.export("living_dimmer", sdc.dial {
  label = "Living room",
  render = function()
    local l = ha.state("light.living_room")
    local pct = l and l.attributes.brightness and math.floor(l.attributes.brightness / 2.55 + 0.5) or 0
    return { title = "Living", value = (l and l.state == "on") and (pct .. "%") or "off",
             indicator = pct, icon = "icons/bulb.png" }
  end,
  rotate = function(ctx, ticks)
    ha.call("light", "turn_on", { entity_id = "light.living_room", brightness_step_pct = ticks * 5 })
  end,
  press = function() ha.call("light", "toggle", { entity_id = "light.living_room" }) end,
})
```

Ids are 1-64 characters of `A-Z a-z 0-9 _ - .` and must be unique (a bad id fails the load). A dial cannot sit in
a page; it is export-only. Full details in [crates/server/README.md](../crates/server/README.md#exports-and-dials-elgato-plugin).

## Devices

Pages are global. A *device* is a named set of them: it says which pages a deck can show, where it starts, and
optionally its own brightness and idle settings. Use devices when several decks share one config, for example a
Stream Deck Plus in the office that needs different pages than the Mk.2 in the living room. A config without
`sdc.device` behaves exactly as described above: every deck sees every page.

```lua
sdc.device("living-room", {
  pages = { "home", "media" },     -- page names, which must exist; order does not matter
  home = "home",                   -- start page, must be in pages (default: the first of pages)
  brightness = 60,                 -- optional, like sdc.brightness
  idle = { dim_after = 120, off_after = 600, home_after = 3600 },   -- optional, like sdc.idle
})
sdc.device("office", { pages = { "home" } })

sdc.default_device { mk2 = "living-room", plus = "office", ["*"] = "living-room" }
```

`sdc.default_device` maps a device type to a device name. The types, derived from the model name a client reports,
are `original` (Original and OriginalV2), `mk2` (including its scissor and module variants), `mini`, `xl`, `plus`,
`plus-xl`, `neo` and `pedal`, plus `browser` for [browser decks](browser.md). `"*"` is the fallback for anything
else.

**Which device a client gets**, in order:

1. The device it asked for by name: `sdc-client --device <name>` or `SDC_DEVICE` on the Pi (see
   [Raspberry Pi deck](pi.md#configuring-the-client)), or the Layout picker in the [browser deck](browser.md) menu. An unknown
   name is logged as a warning and skipped.
2. The `sdc.default_device` entry for the client's type.
3. The `"*"` entry.
4. The first `sdc.device` defined.
5. With no devices defined at all, the legacy behaviour: every page, `sdc.default_page`, the global settings.

Details:

- A device's `brightness` and `idle` replace the global `sdc.brightness` and `sdc.idle` when set (the `idle` table as
  a whole, not field by field). `sdc.default_page` is only used when there are no devices.
- `ctx.goto_page` to a page outside the client's device is logged and ignored.
- The device's `home` is the start page, the target of `home_after`, and the fallback when a remembered page is not
  on the device (also after a config reload). A deck that reconnects and lands on a different device than before
  starts at the new device's home.
- A config reload re-resolves the device of every connected client. A mirror viewer uses the mirrored deck's device.
  Elgato plugin exports are not affected by devices.
- These fail the load, keeping the previous config: an unknown page in `pages`, a `home` not in `pages`, duplicate
  device names, or `sdc.default_device` naming an unknown device or type.

## Feedback & navigation markers

**Press feedback.** After a `press` handler finishes without a Lua error, the key briefly shows a white ring (400 ms)
on physical decks, browser decks and Elgato plugin keys, so you can tell a press was accepted before Home Assistant
answers. It is on by default and can be turned off globally or per button:

```lua
sdc.feedback(false)                                   -- off everywhere
sdc.button { render = { text = "Quiet" }, feedback = false, press = function() end }   -- off for this button
```

It does not flash for a press that only woke the display, a press that called `ctx.goto_page` (the page change is the
feedback), a button without `press`, or a span.

**Navigation chevron.** `chevron = true` (or `"forward"`) in a render descriptor draws a faint "›" in the
bottom-right corner of a key that opens another page; `chevron = "back"` draws "‹" in the top-left for a key that
goes back (e.g. Home). On an icon-only key the icon shrinks slightly and moves away from the chevron so it has room.

```lua
sdc.button {
  render = { text = "Media", icon = "icons/play.png", bg = "#3b2f6b", chevron = true },
  press  = function(ctx) ctx.goto_page("media") end,
}
```

## Putting it together

A small but complete config:

```lua
local sdc = require("sdc")

sdc.brightness(60)
sdc.idle { dim_after = 120, off_after = 600, home_after = 3600 }

sdc.page("home", {
  sdc.button {
    render = function()
      local l = ha.state("light.office")
      local on = l and l.state == "on"
      return { text = "Office", icon = "icons/bulb.png", bg = on and "#f5b400" or "#2a2a2e" }
    end,
    press = function() ha.call("light", "toggle", { entity_id = "light.office" }) end,
  },
})
```

Run it with `HA_URL=https://homeassistant.local:8123 HA_TOKEN=... sdc-server config.lua`, or use the Docker image
described in [Deployment](deploy.md).
