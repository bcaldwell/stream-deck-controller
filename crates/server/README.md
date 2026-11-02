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
| `SDC_LISTEN` | `0.0.0.0:7700`   | Client WebSocket listen address (path `/ws`) |
| `SDC_CLIENT_DIR` | `/usr/local/share/sdc/clients` | Dir of `sdc-client-<target>` binaries to distribute; missing = feature off |
| `SDC_CLIENT_TOKEN` | - | Shared secret: `/ws` and `/client/*` need `Authorization: Bearer <token>`. Unset = open (logged as a warning) |
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
sdc.default_page("home")          -- optional; default is the first page defined
sdc.brightness(60)                -- optional; active brightness 0-100 (sent on connect and on wake)
sdc.key_gap(18)                   -- optional; default gap between keys for spans (px at 72 px keys, default 18)
sdc.idle { dim_after = 120, dim_brightness = 10, off_after = 600, home_after = 3600 }   -- optional, seconds
```

- Render descriptor: `text` (`\n` allowed; wraps and auto-shrinks), `bg`/`fg` (`#rgb`, `#rrggbb`),
  `icon` and `image` (see [Icons and images](#icons-and-images)), `font_size` (px; auto-fit when absent).
- `ctx`: `client_id`, `key` (0-based), `page`, `rows`, `cols`, `goto_page(name)`; for spans also
  `span_cols`, `span_rows` and, in `press`/`release`, `span_index` (see [Spans](#spans)).
- `ha` (also `sdc.ha`): `ha.state(entity_id)` and fire-and-forget `ha.call(domain, service, data)`.
- Dependency tracking is automatic: every entity passed to `ha.state` during a render is recorded
  for that (client, key). A `state_changed` re-renders only those keys, and a frame is sent only if
  the encoded image actually changed.
- A Lua error in `render`/`press` is logged and shows a red `ERR` tile; the server keeps running.
- Other `.lua` files next to the config can be loaded with `require`.
- Idle: without `sdc.idle` a deck never idles. Otherwise, after `dim_after` seconds without a key press the
  server sends brightness `dim_brightness` (default 10), and after `off_after` seconds brightness 0
  (backlight off; key images are left alone). Both are optional. The next key press restores the active
  brightness (`sdc.brightness`, default 60) and is consumed: it only wakes the deck, its press and release
  handlers do not run. Any press while awake restarts the timer. Clients are checked once a second; a config
  reload re-reads the settings without waking or dimming anyone (a changed setting takes effect on the next check).
- Pages: the server remembers each client's page by `client_id` (in memory), so a client that reconnects (server
  deploy, client self-update) returns to the page it was on, unless that page no longer exists. Waking an idle
  deck also keeps its page. `home_after` (optional) switches a client to the default page after that many seconds
  without a key press, rendering it even while the display is off, so waking shows home; a disconnected client's
  remembered page expires after the same time. Without `home_after` pages are remembered until the server restarts.
- Rendering: Lua `render` functions run one after the other, then the keys of a page (connect, page switch,
  reload) are rasterized and JPEG-encoded in parallel. Pictures are premultiplied and shrunk (shorter side
  <= 256 px) when decoded, and scaled once per key size and cached until the next config reload.
  `RUST_LOG=sdc_server=debug` logs how long each render took.

## Icons and images

- `icon`: a picture fitted inside the key (with padding, aspect ratio kept) over `bg`; with `text`, the
  picture takes the top half and the text the bottom half. Transparent PNGs show `bg` through.
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

`sdc.span { cols, rows, render, press, release, gap }` is a button that covers a rectangle of keys and shows
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
- `render` returns the usual descriptor, laid out once on a canvas of `cols*key + (cols-1)*gap` by
  `rows*key + (rows-1)*gap` px, which then is cut into one tile per key (tile (c, r) starts at
  `(c*(key+gap), r*(key+gap))`), so the picture continues across the physical space between keys. `image`
  covers the whole canvas (center-cropped to its aspect); `icon`/`text` are placed on the whole canvas, with
  padding and text size following its shorter side.
- `gap` (per span) or `sdc.key_gap(px)` (global) is in px at 72 px keys and scales with the client's key size;
  the default is 18 (0.25 of a key, about the pitch of a Stream Deck MK.2).
- Dependencies: entities read by a span's `render` and its image downloads re-render all of its keys. Each tile
  is still only sent when its JPEG changed. Scaled pictures are cached per canvas size.
- `ctx` in `render` has `key` = the span's top-left key; in `press`/`release`, `key` is the pressed key and
  `span_index` its position in the span. Idle wake, `home_after` and page memory work as for buttons.

## Layout

`ha.rs` HA websocket client (channel boundary), `engine.rs` event loop / per-client state /
dependency map, `lua_api.rs` Lua bindings, `render.rs` JPEG key renderer (embedded Open Sans, OFL;
see `assets/OFL.txt`), `assets.rs` image sources (files, URLs, S3 presigning) and the background fetcher, `ws.rs` client WebSocket, `http.rs` routes (client downloads, health) and token check, `clients.rs` distributed binaries, `watch.rs` hot reload, `preview.rs` `render` command.
