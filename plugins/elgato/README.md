# Stream Deck Controller plugin for Elgato's Stream Deck app

Use buttons and dials defined in your stream-deck-controller Lua config on a Stream Deck attached to a
laptop running Elgato's Stream Deck app, mixed with normal Elgato actions. It also works with the dials and
touch strip of a Stream Deck +.

The plugin is a client of the server (a second one next to `sdc-client`): it connects to `/plugin/ws` with the
same bearer token, subscribes each visible action to an export and shows what the server renders. Presses and dial
input go back to the server's Lua handlers. Everything else (page switching, idle, brightness) stays with the
Elgato app.

Two actions appear in the Stream Deck app under "Stream Deck Controller":

- **SDC Button** (keys): shows the exported button's image, runs its `press`/`release`.
- **SDC Dial** (Stream Deck + dials): shows title, value, indicator bar and icon, runs `rotate`, `press`,
  `release` and `touch` (tap on the touch strip).

## Install

Needs Elgato's Stream Deck app 7.1 or newer (macOS 12+, Windows 10+).

1. Get `ca.bcaldwell.sdc.streamDeckPlugin`: from the GitHub release / the `elgato-plugin` artifact of a CI run,
   or build it (below).
2. Double-click it; the Stream Deck app installs it.
3. Drag "SDC Button" or "SDC Dial" onto a key or dial. In its settings:
   - **Server URL** (shared by all actions): your server, e.g. `your-server.example.com` (a bare host or `https://` URL means `wss://host/plugin/ws`).
     Nothing connects until it is set.
   - **Token** (shared): the server's `SDC_CLIENT_TOKEN`. Stream Deck stores plugin global settings in its
     preferences as plain text, so treat the laptop like any other place you keep that token. Leave empty if the
     server has no token.
   - **Export**: a dropdown with the server's exports of the right kind (buttons for a key, dials for a dial).
     The list is empty while the server is unreachable.

The first connection can take a moment; keys keep their last image while the server is away and flash the
alert (yellow triangle) when pressed offline. The plugin reconnects with backoff (1 s up to 30 s), re-subscribes
every visible action, and reconnects immediately after the computer wakes from sleep.

## Exporting from Lua

Only exported things are visible to the plugin (`crates/server/README.md` has the full reference):

```lua
sdc.export("living_light", toggle("light.living_room", "Living", "#a87308"))   -- any sdc.button / sdc.span

sdc.export("living_dimmer", sdc.dial {
  label = "Living room",                              -- optional, shown in the dropdown
  render = function(ctx)
    local l = ha.state("light.living_room")
    local pct = l and l.attributes.brightness and math.floor(l.attributes.brightness / 2.55 + 0.5) or 0
    return { title = "Living", value = (l and l.state == "on") and (pct .. "%") or "off",
             indicator = pct, icon = "icons/bulb.png", bg = "#000000" }
  end,
  rotate = function(ctx, ticks)                       -- ticks are batched over ~100 ms; may be negative
    ha.call("light", "turn_on", { entity_id = "light.living_room", brightness_step_pct = ticks * 5 })
  end,
  press = function(ctx) ha.call("light", "toggle", { entity_id = "light.living_room" }) end,
})
```

- `sdc.export(id, item)` returns `item`, so you can also put the same button in a page.
- A span exports as a single key. `ctx.goto_page` does nothing for plugin slots.
- Render functions track Home Assistant dependencies like on a deck: only the slots that read a changed entity
  are re-rendered, and only changed images are sent. Saving the config reloads it; slots whose export vanished
  show an error tile (keys) or `ERR` (dials) and the dropdown list updates.

## Build, pack, develop

Node 20.5+ (the repo's `nix develop` has it; the Stream Deck app runs the plugin on its own bundled Node 24).

```sh
task plugin:build     # npm ci, typecheck, rollup -> ca.bcaldwell.sdc.sdPlugin/bin/plugin.js
task plugin:pack      # + streamdeck validate/pack -> ca.bcaldwell.sdc.streamDeckPlugin
task plugin:link      # build and `streamdeck link` it into the local Stream Deck app (restart it with `npm run watch`)
```
(or `npm run build|pack|link|watch|typecheck` in this directory.) Plugin logs are in
`ca.bcaldwell.sdc.sdPlugin/logs/` (linked install) or the Stream Deck app's log folder.

Layout: `src/hub.ts` the single websocket (reconnect, subscriptions, tick batching; no Stream Deck imports),
`src/actions/` the two actions (a shared base subscribes them and feeds the property inspector's dropdown), `src/protocol.ts` the message types
(mirror of `crates/protocol`), `ca.bcaldwell.sdc.sdPlugin/` manifest, property inspector pages (`ui/`, built on
the sdpi-components script from Elgato's CDN, like Elgato's own template) and icons (`scripts/gen-icons.py`
regenerates them, stdlib only).
