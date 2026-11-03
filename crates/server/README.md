# sdc-server

Stream Deck controller server. Thin clients (a Stream Deck attached to a Pi or laptop) connect over
WebSocket; the server runs your Lua config, tracks Home Assistant state, renders key images and
pushes them to every connected client. Each client has its own current page.

```
sdc-server [CONFIG]                       # run the server
sdc-server render [--config f] [--out dir] [--page name] [--states states.json]
```

`render` writes `key_NN.png` for each key plus a stitched `page.png` (3x5, 72px) so you can check a
layout without hardware. It uses real Home Assistant if `HA_URL`/`HA_TOKEN` are set, otherwise empty
states (or the fake ones from `--states`, a JSON list of `{entity_id, state, attributes}`).

## Environment

| Variable     | Default          | Meaning |
|--------------|------------------|---------|
| `SDC_CONFIG` | `./config.lua`   | Lua config path (first CLI argument overrides) |
| `HA_URL`     | -                | e.g. `http://homeassistant.local:8123` (`/api/websocket` and ws/wss derived) |
| `HA_TOKEN`   | -                | Long-lived access token |
| `SDC_LISTEN` | `0.0.0.0:7700`   | Listen address (clients on `/ws`, the Elgato plugin on `/plugin/ws`) |
| `SDC_CLIENT_DIR` | `/usr/local/share/sdc/clients` | Dir of `sdc-client-<target>` binaries to distribute; missing = feature off |
| `SDC_DECK_AUTH` | (SSO header required) | `none` disables the `/deck` login check (local development only; logged as a warning) |
| `SDC_CLIENT_TOKEN` | - | Shared secret: `/ws`, `/plugin/ws` and `/client/*` need `Authorization: Bearer <token>`. Unset = open (logged as a warning) |
| `SDC_ASSETS_S3_ENDPOINT` | - | S3 endpoint for icon objects, e.g. `http://garage.garage.svc.cluster.local:3900` (in cluster) or `https://s3.example.com` |
| `SDC_ASSETS_S3_BUCKET` | - | Bucket holding the images, e.g. `stream-deck-icons` |
| `SDC_ASSETS_S3_REGION` | `garage` | S3 region used for signing |
| `SDC_ASSETS_S3_ACCESS_KEY_ID` / `SDC_ASSETS_S3_SECRET_ACCESS_KEY` | - | Credentials (read access is enough). All four non-region vars are needed to enable S3 |
| `RUST_LOG`   | `info`           | tracing filter |

Without `HA_URL`/`HA_TOKEN` (or if HA is down) the server still serves clients; `ha.state` returns
`nil` and `ha.call` is dropped. The HA connection reconnects with backoff and reseeds all states.
The config file is hot-reloaded on save; a config that fails to load is logged and the previous one
stays active.

## HTTP endpoints and client updates

Besides the `/ws` WebSocket, the same port serves `GET /client/<target>` (client binary, 404 if the
server has none), `GET /client/<target>.sha256` and `GET /healthz`. Binaries are read and hashed once
at startup. When a client's `Hello` reports a target we have a binary for and a different sha256, the
server sends `Update { path, sha256 }` right after `Welcome` and the client replaces itself. With
`SDC_CLIENT_TOKEN` set, `/ws` and `/client/*` answer 401 unless the request has
`Authorization: Bearer <token>` (compared in constant time); `/healthz` is always open. Install on a
Pi with `task pi:install` from the repo (see `deploy/pi/README.md`).

## Browser deck

`GET /deck` serves a Stream Deck in the browser: dark MK.2 body, 3x5 glossy keys that depress on
press, one self-contained page (no build step, no CDN) plus a web manifest (landscape, fullscreen,
installable on a phone). It speaks the normal client protocol over `/deck/ws`.

- **Own deck** (default): the browser is a client of its own (`client_id` `web-<random>`, kept in
  localStorage) with its own page, idle state and brightness, rendered at 144 or 192 px per key
  depending on `devicePixelRatio`.
- **Layout**: when the config has `sdc.device`s, the dot on the bezel opens a menu listing "Default" plus
  every device name (`GET /deck/api/devices`: `[{name, pages, home}]`, same SSO auth, `[]` without
  devices). The choice is kept in localStorage and sent as `Hello.device_name`; changing it reconnects.
- Phones: a portrait viewport rotates the deck 90 degrees; the bezel has a fullscreen button
  (locks landscape where supported). The LED shows the connection (green, or pulsing amber while
  reconnecting with backoff).

Auth: every `/deck` route needs the `X-Auth-Request-Email` header that the cluster's SSO (Traefik
forwardAuth to oauth2-proxy) sets on authenticated requests, and answers 401 without it; the email
is logged when a deck connects. The Traefik middleware must be the only way to reach these routes
(the header is trusted). `SDC_DECK_AUTH=none` skips the check for local use. The bearer token of
`/ws` and `/client/*` is unrelated: it neither opens `/deck/*` nor does the SSO header open `/ws`.
`/deck/ws` also refuses cross-origin upgrades and forces browser ids to start with `web-`, so a
browser cannot take over a physical deck's identity.

## Lua API

```lua
local sdc = require("sdc")

sdc.page("home", {                -- list index = key index, row-major, key 1 (Lua) = top-left
  sdc.button {
    render = function(ctx)        -- or a static table / string
      local l = ha.state("light.office")      -- {entity_id, state, attributes} or nil
      return { text = "Office", bg = (l and l.state == "on") and "#f5b400" or "#222222",
               fg = "#ffffff", icon = "icons/bulb.png", font_size = 18 }
    end,
    press   = function(ctx) ha.call("light", "toggle", { entity_id = "light.office" }) end,
    release = function(ctx) end,              -- optional
  },
  false,                          -- empty key (nil/false)
  sdc.button { render = { text = "Media" }, press = function(ctx) ctx.goto_page("media") end },
})
sdc.device("deck", { pages = { "home" } })   -- required: at least one; brightness and idle are set here (see Devices)
sdc.feedback(true)                -- optional; flash a key briefly after its press ran OK (default true)
```

- Render descriptor: `text` (`\n` allowed; wraps and auto-shrinks), `bg`/`fg` (`#rgb`, `#rrggbb`),
  `icon` and `image` (see [Icons and images](#icons-and-images)), `tint` (`#rrggbb`, recolours the icon), `font_size` (px at a 72 px key, scaled with the key's size; auto-fit when absent),
  `chevron = true | "forward" | "back"` (a faint "›" in the bottom-right corner for a key that opens another page, or "‹" in the top-left for one that goes back; on an icon-only key the icon shrinks slightly and moves away from it).
- Press feedback: after a `press` handler finishes without a Lua error, the key shows a white ring for 400 ms (physical, browser and
  Elgato plugin keys). Not shown for presses that only wake the display, that call `ctx.goto_page`, for buttons without `press`, for
  spans, or when turned off globally with `sdc.feedback(false)` or per button with `feedback = false` in `sdc.button{}`.
- `ctx`: `client_id`, `key` (0-based), `page`, `rows`, `cols`, `goto_page(page)` (a page handle or name); for spans also
  `span_cols`, `span_rows` and, in `press`/`release`, `span_index` (see [Spans](#spans)).
- `ha` (also `sdc.ha`): `ha.state(entity_id)` and fire-and-forget `ha.call(domain, service, data)`.
- Dependency tracking is automatic: every entity passed to `ha.state` during a render is recorded
  for that (client, key). A `state_changed` re-renders only those keys, and a frame is sent only if
  the encoded image actually changed.
- A Lua error in `render`/`press` is logged and shows a red `ERR` tile; the server keeps running.
- Other `.lua` files next to the config can be loaded with `require`.
- Idle (the `idle` table of an `sdc.device`): without it a deck never idles. Otherwise, after `dim_after` seconds without a key press the
  server sends brightness `dim_brightness` (default 10), and after `off_after` seconds brightness 0
  (backlight off; key images are left alone). Both are optional. The next key press restores the active
  brightness (the device's `brightness`, default 60, sent on connect and on wake) and is consumed: it only wakes the deck, its press and release
  handlers do not run. Any press while awake restarts the timer. Clients are checked once a second; a config
  reload re-reads the settings without waking or dimming anyone (a changed setting takes effect on the next check).
- Pages: the server remembers each client's page by `client_id` (in memory), so a client that reconnects (server
  deploy, client self-update) returns to the page it was on, unless that page no longer exists. Waking an idle
  deck also keeps its page. `home_after` (optional) switches a client to its device's home page after that many seconds
  without a key press, rendering it even while the display is off, so waking shows home; a disconnected client's
  remembered page expires after the same time. Without `home_after` pages are remembered until the server restarts.
- Rendering: Lua `render` functions run one after the other, then the keys of a page (connect, page switch,
  reload) are rasterized and JPEG-encoded in parallel. Pictures are premultiplied and shrunk (shorter side
  <= 256 px) when decoded, and scaled once per key size and cached until the next config reload.
  `RUST_LOG=sdc_server=debug` logs how long each render took.

## Devices

Pages are global; a *device* picks which of them a deck shows, where it starts and how it idles.

```lua
local home = sdc.page("home", { ... })   -- sdc.page / sdc.device return handles; names work too
local living = sdc.device("living-room", {
  pages = { home, "lights", "music" },  -- page handles or names (must exist); order irrelevant
  home = home,                          -- start/default page, must be in pages (default: the first of pages)
  brightness = 60,                      -- optional, 0-100 (default 60)
  idle = { dim_after = 120, dim_brightness = 10, off_after = 600, home_after = 3600 }, -- optional; absent: never idles
})
sdc.default_device { mk2 = living, plus = "office-plus", ["*"] = living }   -- device handles or names
```

- Device types for `sdc.default_device`: `original` (Original, OriginalV2), `mk2` (Mk2 and its scissor/module
  variants), `mini`, `xl`, `plus`, `plus-xl`, `neo`, `pedal`, derived from the model name the client sends, and
  `browser` for `/deck` clients. `"*"` is the fallback.
- A connecting client gets, in order: the device it asked for (`sdc-client --device <name>` /
  `SDC_DEVICE`, `Hello.device_name`; an unknown name is logged as a warning and skipped), the
  `sdc.default_device` entry for its type, the `"*"` entry, the first `sdc.device` defined.
- A config must define at least one `sdc.device` (otherwise it fails to load). `sdc.default_page`, `sdc.idle` and
  `sdc.brightness` were removed (calling them is an error); a device's `idle` and `brightness` replace them, and a
  device without them never idles and uses brightness 60. `sdc.feedback` stays global.
- `ctx.goto_page` to a page outside the client's device is logged and ignored. The device's `home` is the start page, the
  target of `home_after` and the fallback when a remembered page is not on the device (also after a config
  reload). A reconnecting client that lands on a different device than before starts at that device's home.
- Config reload re-resolves every connected client's device. The
  Elgato plugin (`sdc.export`) is not affected.
- Config errors fail the load (the previous config stays active): unknown page in `pages`, `home` not in
  `pages`, duplicate device names, `sdc.default_device` naming an unknown device or type.

## Exports and dials (Elgato plugin)

`sdc.export(id, item)` makes a button, span or dial available to the Elgato Stream Deck plugin
(`plugins/elgato`) and returns `item` unchanged, so the same button can also sit in a page. Only exported
things are visible to plugins. `id` is 1-64 characters of `A-Z a-z 0-9 _ - .` and must be unique; an invalid or
duplicate id fails the config load (the previous config stays active). An optional `label = "..."` field on the
button or dial is shown in the plugin's dropdown.
```lua
sdc.export("living_light", toggle("light.living_room", "Living", "#a87308"))

sdc.export("living_dimmer", sdc.dial {
  label = "Living room",
  render = function(ctx)                       -- required; same dependency tracking as a button
    local l = ha.state("light.living_room")
    local pct = l and l.attributes.brightness and math.floor(l.attributes.brightness / 2.55 + 0.5) or 0
    return { title = "Living", value = (l and l.state == "on") and (pct .. "%") or "off",
             indicator = pct, icon = "icons/bulb.png", bg = "#000000" }
  end,
  rotate  = function(ctx, ticks)               -- once per plugin message (ticks batched over ~100 ms), may be negative
    ha.call("light", "turn_on", { entity_id = "light.living_room", brightness_step_pct = ticks * 5 })
  end,
  press   = function(ctx) ha.call("light", "toggle", { entity_id = "light.living_room" }) end,
  release = function(ctx) end,                 -- optional
  touch   = function(ctx) end,                 -- optional: tap on the touch strip
})
```
- `sdc.light_dial { entity, label?, icon?, modes?, step? }` and `sdc.volume_dial { entity, label?, icon?, step? }`
  are built-in dials (`crates/server/src/lua/dials.lua`). While being turned they step from their own value, not
  HA's lagging state, send absolute values and show their value at once; `DIAL_SETTLE` (1.5 s) after the last turn
  the engine redraws the slot and HA's state shows again. A light dial's press cycles brightness, warmth and hue
  (those the light's `supported_color_modes` allow) and a tap toggles; a volume dial's press mutes. `sdc.now()`
  gives milliseconds on a monotonic clock.
- `sdc.dial { render, rotate, press, release, touch }`: all handlers optional except `render`. `render` returns
  `title`, `value` (strings or numbers), `indicator` (0-100, clamped), `icon` (same sources as button icons) and
  `bg` (`#rrggbb`, behind the icon; default black). A Lua error shows `ERR` as the title and is logged. A dial
  cannot be placed in a page (the config fails to load with a message saying so); it is only for export.
- Exported keys render at the size the plugin subscribes with (144 px) as PNG, using the usual descriptor. An
  exported `sdc.span` renders as a normal single key (the span size is ignored and `ctx.span_*` are absent).
- `ctx` for exports has `client_id` (the plugin's id) and `slot` (the Elgato action instance); `key`, `page`, `rows`
  and `cols` are absent. `ctx.goto_page` does nothing there (logged at debug level): plugins have no pages.
- Every subscribed slot has its own render, dependencies and "changed" check, so a state change re-renders and
  sends only the slots that read that entity, and only if the image actually changed. A config reload re-resolves
  every slot by export id: a removed export (or one that changed kind) gives that slot a `SlotError`.
- A dial slot is also re-rendered after each of its handlers runs, so a dial can keep its own state in Lua (keyed by
  `ctx.slot`, e.g. a mode that `press` cycles) and show it right away.

## Plugin websocket (`/plugin/ws`)

Same bearer-token auth as `/ws` (`SDC_CLIENT_TOKEN`); the SSO header does not open it. JSON text frames, defined
in `crates/protocol` (`PluginClientMessage`, `PluginServerMessage`); `PROTOCOL_VERSION` stays 1. `slot` is the
Elgato action context id.

| Direction | Message |
|-----------|---------|
| plugin -> server | `plugin_hello {protocol_version, client_id}` first; `subscribe {slot, export, kind: "key"\|"dial", size_px}` (replaces an existing subscription of the slot); `unsubscribe {slot}`; `slot_down/slot_up {slot}`; `dial_rotate {slot, ticks}`; `dial_down/dial_up {slot}`; `touch_tap {slot}`; `list_exports` |
| server -> plugin | `plugin_welcome`; `exports {items: [{id, kind, label?}]}` (after the welcome, on `list_exports` and on every config reload); `key_image {slot, png_base64}`; `dial_feedback {slot, title, value, indicator, icon_png_base64?}`; `slot_error {slot, message}`; `error {message}` |

Images and feedback are only sent when they differ from what that slot last got. Input for a slot of the other
kind, or for an unknown slot, is ignored. A press handler that raises an error turns that key's image into the
red ERR tile until its next render. A connection may hold 256 slots; `size_px` must be 1-1024.

## Icons and images

- `icon`: a picture fitted inside the key (with padding, aspect ratio kept) over `bg`; with `text`, the
  picture takes the top ~58% of the key and the text sits in a band along the bottom edge (a label that
  wraps onto two lines gets a taller band, so the icon shrinks a little). Transparent PNGs show `bg` through. `tint = "#rrggbb"` recolours the icon's visible pixels to that colour,
  keeping their alpha (an icon without transparency uses its luminance as alpha); it never affects `image`.
- `image`: a full-bleed picture, scaled to cover the whole key (center-cropped). `text` is drawn on top
  along the bottom, over a subtle dark gradient and shadow so it stays legible. `icon` can still be
  combined with it.
- Both take the same string, resolved in this order: `http://` / `https://` URL (downloaded); a path starting
  with `/api/` (fetched from Home Assistant, see below); else a file that exists relative to the config directory; else, when the `SDC_ASSETS_S3_*` variables are set, an
  object key in the assets bucket (path-style addressing, presigned GET requests); else an error.
  ```lua
  { text = "Movies", image = "tv/popcorn.png" }                         -- bucket object
  { text = "Lamp", icon = "https://example.com/bulb.png", bg = "#FFA000" }
  ```
- Home Assistant paths: a source starting with `/api/` is fetched from `HA_URL` + the path with
  `Authorization: Bearer <HA_TOKEN>`; without `HA_URL`/`HA_TOKEN` it is an error source. This is how media
  artwork works (`attributes.entity_picture` is such a path and changes with every track):
  ```lua
  sdc.button { render = function()
    local p = ha.state("media_player.living_room_2")
    local art = p and p.state == "playing" and p.attributes.entity_picture
    return art and { image = art } or { icon = "tv/play_pause.png", bg = "#000000" }
  end }
  ```
  Artwork that cannot be fetched (404, player stopped) is logged at debug level and rendered as if no
  picture was given, not as an `ERR` tile; `image = nil` is likewise just no picture.
- Rendering never waits for the network. Downloaded images are cached decoded in memory (the 128 most recently
  rendered, failed downloads included, so changing sources such as artwork do not pile up); a key whose image is not there yet renders without it, and is re-rendered as soon as the
  download completes. A failed download is logged once and shown as a red `ERR` tile in the picture's area
  only (icon area, or the top half of the key for `image`); it is retried after 60 s, or on a config reload.
  Static `icon`/`image` strings in the config are prefetched on every (re)load. `sdc-server render` waits
  for downloads before writing the PNGs.

## Spans

`sdc.span { cols, rows, render, press, release }` is a button that covers a rectangle of keys and shows
one picture or text across all of them, e.g. album art over 2x2 keys. The page list is row-major key slots: a
span in slot k occupies k..k+cols-1 on that row and the same columns on the following rows, and the other
slots it covers must be `false`/`nil`.
```lua
sdc.page("music", {
  sdc.button { render = { text = "Back" } },       -- key 0
  sdc.span { cols = 2, rows = 2,                   -- keys 1, 2, 6, 7
    render = function(ctx)
      local p = ha.state("media_player.living_room_2")
      local art = p and p.attributes.entity_picture
      return art and { image = art } or { text = "Nothing playing" }
    end,
    press = function(ctx) print(ctx.key, ctx.span_index) end },   -- pressed key; 0..n-1 row-major
  false, false, false,                             -- keys 3, 4, 5
  false, false, false,                             -- keys 8, 9, 10 (key 6, 7 are the span's second row)
})
```
- The grid size is per client, so spans are placed when a client renders: a span that runs off the client's
  rows/cols, or covers a slot holding a button or another span, is logged and shows the red `ERR` tile on its
  keys (as does a Lua error in its `render`). Pressing an ERR span does nothing.
- `render` returns the usual descriptor, laid out once on a canvas of `cols*key` by `rows*key` px, which then is cut into
  one tile per key, so the picture continues across the keys. `image`
  covers the whole canvas (center-cropped to its aspect); `icon`/`text` are placed on the whole canvas, with
  padding and text size following its shorter side.
- Dependencies: entities read by a span's `render` and its image downloads re-render all of its keys. Each tile
  is still only sent when its JPEG changed. Scaled pictures are cached per canvas size.
- `ctx` in `render` has `key` = the span's top-left key; in `press`/`release`, `key` is the pressed key and
  `span_index` its position in the span. Idle wake, `home_after` and page memory work as for buttons.

## Layout

`ha.rs` HA websocket client (channel boundary), `engine.rs` event loop / per-client state /
dependency map, `lua_api.rs` Lua bindings, `render.rs` JPEG key renderer (embedded Open Sans, OFL;
see `assets/OFL.txt`), `assets.rs` image sources (files, URLs, S3 presigning) and the background fetcher, `ws.rs` client and plugin WebSockets, `engine/plugin.rs` plugin slots (subscriptions, per-slot dependencies), `http.rs` routes (client downloads, health) and token check, `clients.rs` distributed binaries, `watch.rs` hot reload, `preview.rs` `render` command.
