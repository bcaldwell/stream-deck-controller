# Elgato plugin

If you already use Elgato's Stream Deck app on a laptop, the SDC plugin lets you place buttons and dials from your Lua
config next to ordinary Elgato actions, including the dials and touch strip of a Stream Deck +. The plugin is another
client of the server: it connects to `/plugin/ws` with the same bearer token, subscribes each visible action to an
export, and shows what the server renders. Presses and dial input run your Lua handlers. Page switching, idle and
brightness stay with the Elgato app.

Three actions appear under "Stream Deck Controller": **SDC Button** (one exported key), **SDC Dial** (dials, with
title, value, indicator bar and icon) and **SDC Deck Key** (a key of a whole page set, see
[Whole pages](#whole-pages-sdc-deck-key)).

## Quick start

1. In `config.lua`, export what you want to use ([Exports and dials](configuration.md#exports-and-dials-elgato-plugin)):

   ```lua
   sdc.export("living_light", light("light.living_room", "Living"))
   ```

2. Install `ca.bcaldwell.sdc.streamDeckPlugin` (from a GitHub release, or the `elgato-plugin` artifact of a CI run,
   or `task plugin:pack`) by double-clicking it. Needs the Stream Deck app 7.1 or newer.
3. Drag **SDC Button** or **SDC Dial** onto a key or dial and set the **Server URL** (`wss://your-server/plugin/ws`),
   the **Token** (`SDC_CLIENT_TOKEN`) and the **Export**.

Only exported things are visible to the plugin (SDC Deck Key excepted). The token is stored in the Stream Deck app's preferences as plain
text, so treat the laptop accordingly.

## Whole pages (SDC Deck Key)

SDC Button shows single exports; **SDC Deck Key** makes the Stream Deck behave like a Pi deck: the server runs the
page set with `goto_page`, home, spans and press flash, and the plugin only shows its key images and sends presses.

1. Define a device for it and make it the default for the deck type (a Stream Deck + is type `plus`; the plugin
   reports the deck it is on, so a normal Stream Deck is `mk2`, XL `xl`, and so on):

   ```lua
   local OFFICE_PLUS = sdc.device("office-plus", { ... })      -- see Devices in configuration.md
   sdc.default_device { plus = OFFICE_PLUS }
   ```

   Or leave `sdc.default_device` alone and type `office-plus` into **Deck layout** in the plugin.
2. In the Stream Deck app drag **SDC Deck Key** onto every key you want the server to drive (all 8 on a Stream Deck +).
   The key's index comes from its position (row by row, 4 columns on a +), so the key in row 2, column 1 is key 4.
   Keys without an SDC Deck Key are not shown by the server; ordinary Elgato actions can sit beside them.
3. **Deck layout** (shared setting, optional) is the `sdc.device` name to request; empty means the server's default
   for the deck type.

Each physical Stream Deck with at least one SDC Deck Key gets its own connection to `/ws` (the server URL setting
works for both `/ws` and `/plugin/ws`), with the client id `elgato-<host>-<device id>`, and its own current page; the
connection closes when no deck key is visible on that deck. Dials are not part of this: use SDC Dial with exports.
Brightness and idle stay with the Elgato app. No profile is bundled; place the keys by hand once.

Installation details and building the plugin are in
[plugins/elgato/README.md](../plugins/elgato/README.md); the wire protocol is in
[crates/server/README.md](../crates/server/README.md#plugin-websocket-pluginws).
