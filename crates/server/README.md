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
| `RUST_LOG`   | `info`           | tracing filter |

Without `HA_URL`/`HA_TOKEN` (or if HA is down) the server still serves clients; `ha.state` returns
`nil` and `ha.call` is dropped. The HA connection reconnects with backoff and reseeds all states.
The config file is hot-reloaded on save; a config that fails to load is logged and the previous one
stays active.

## HTTP endpoints and client updates

Besides the `/ws` WebSocket, the same port serves `GET /client/<target>` (client binary, 404 if the
server has none), `GET /client/<target>.sha256`, `GET /install.sh` (Pi installer, embedded from
`deploy/pi/install.sh`) and `GET /healthz`. Binaries are read and hashed once at startup. When a
client's `Hello` reports a target we have a binary for and a different sha256, the server sends
`Update { path, sha256 }` right after `Welcome` and the client replaces itself. Install on a Pi with
`curl -fsSL http://<server>:7700/install.sh | sudo sh -s -- --server ws://<server>:7700/ws`.

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
```

- Render descriptor: `text` (`\n` allowed; wraps and auto-shrinks), `bg`/`fg` (`#rgb`, `#rrggbb`),
  `icon` (PNG/JPEG path relative to the config file), `font_size` (px; auto-fit when absent).
- `ctx`: `client_id`, `key` (0-based), `page`, `rows`, `cols`, `goto_page(name)`.
- `ha` (also `sdc.ha`): `ha.state(entity_id)` and fire-and-forget `ha.call(domain, service, data)`.
- Dependency tracking is automatic: every entity passed to `ha.state` during a render is recorded
  for that (client, key). A `state_changed` re-renders only those keys, and a frame is sent only if
  the encoded image actually changed.
- A Lua error in `render`/`press` is logged and shows a red `ERR` tile; the server keeps running.
- Other `.lua` files next to the config can be loaded with `require`.

## Layout

`ha.rs` HA websocket client (channel boundary), `engine.rs` event loop / per-client state /
dependency map, `lua_api.rs` Lua bindings, `render.rs` JPEG key renderer (embedded Open Sans, OFL;
see `assets/OFL.txt`), `ws.rs` client WebSocket, `http.rs` routes (client downloads, installer, health), `clients.rs` distributed binaries, `watch.rs` hot reload, `preview.rs` `render` command.
