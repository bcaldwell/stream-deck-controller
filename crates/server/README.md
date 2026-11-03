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

The config API (`sdc.page`, `sdc.button`, `sdc.span`, `sdc.device`, `sdc.default_device`, `sdc.feedback`, the
render descriptor, `ctx`, `ha.state`/`ha.call`, idle and page memory, icons and images) is described in
[docs/configuration.md](../../docs/configuration.md). Implementation notes:

- Dependency tracking: every entity passed to `ha.state` during a render is recorded for that (client, key). A
  `state_changed` re-renders only those keys, and a frame is sent only if the encoded image actually changed.
- Idle: clients are checked once a second; a config reload re-reads the settings without waking or dimming anyone
  (a changed setting takes effect on the next check). A disconnected client's remembered page expires after
  `home_after`; without it pages are remembered until the server restarts.
- Rendering: Lua `render` functions run one after the other, then the keys of a page (connect, page switch,
  reload) are rasterized and JPEG-encoded in parallel. Pictures are premultiplied and shrunk (shorter side
  <= 256 px) when decoded, and scaled once per key size and cached until the next config reload.
  `RUST_LOG=sdc_server=debug` logs how long each render took.
- Images: downloaded images are cached decoded in memory (the 128 most recently rendered, failed downloads
  included, so changing sources such as artwork do not pile up). S3 uses path-style addressing and presigned GET
  requests. `sdc-server render` waits for downloads before writing the PNGs.

## Exports and dials (Elgato plugin)

`sdc.export(id, item)` makes a button, span or dial available to the Elgato Stream Deck plugin
(`plugins/elgato`) and returns `item` unchanged, so the same button can also sit in a page. Only exported
things are visible to plugins. `id` is 1-64 characters of `A-Z a-z 0-9 _ - .` and must be unique; an invalid or
duplicate id fails the config load (the previous config stays active). An optional `label = "..."` field on the
button or dial is shown in the plugin's dropdown.
See [Exports and dials](../../docs/configuration.md#exports-and-dials-elgato-plugin) for examples.
- `sdc.light_dial { entity, label?, icon? }` and `sdc.volume_dial { entity, label?, icon? }`
  are built-in dials (`crates/server/src/lua/dials.lua`). While being turned they step from their own value, not
  HA's lagging state, send absolute values and show their value at once; `DIAL_SETTLE` (1.5 s) after the last turn
  the engine redraws the slot and HA's state shows again. A light dial's press cycles brightness, warmth and hue
  (those the light's `supported_color_modes` allow) and a tap toggles; a volume dial's press mutes. `sdc.now()`
  gives milliseconds on a monotonic clock.
- `sdc.dial { render, rotate, press, release, touch, label }`: all handlers optional except `render`. `rotate(ctx, ticks)`
  runs once per plugin message (ticks batched over ~100 ms, may be negative); `touch` is a tap on the touch strip. `render` returns
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

## Spans

A span (`sdc.span { cols, rows, render, press, release }`, see
[Spans](../../docs/configuration.md#spans)) is placed when a client renders, because the grid size is per client:
a span that runs off the client's rows/cols, or covers a slot holding a button or another span, is logged and
shows the red `ERR` tile on its keys (as does a Lua error in its `render`). Pressing an ERR span does nothing.

- `render` returns the usual descriptor, laid out once on a canvas of `cols*key` by `rows*key` px and cut into one
  tile per key. `image` covers the whole canvas (center-cropped to its aspect); `icon`/`text` are placed on the
  whole canvas, with padding and text size following its shorter side.
- Entities read by a span's `render` and its image downloads re-render all of its keys. Each tile is still only
  sent when its JPEG changed. Scaled pictures are cached per canvas size.
- `ctx` in `render` has `key` = the span's top-left key; in `press`/`release`, `key` is the pressed key and
  `span_index` its position in the span.

## Layout

`ha.rs` HA websocket client (channel boundary), `engine.rs` event loop / per-client state /
dependency map, `lua_api.rs` Lua bindings, `render.rs` JPEG key renderer (embedded Open Sans, OFL;
see `assets/OFL.txt`), `assets.rs` image sources (files, URLs, S3 presigning) and the background fetcher, `ws.rs` client and plugin WebSockets, `engine/plugin.rs` plugin slots (subscriptions, per-slot dependencies), `http.rs` routes (client downloads, health) and token check, `clients.rs` distributed binaries, `watch.rs` hot reload, `preview.rs` `render` command.
