# Elgato plugin

If you already use Elgato's Stream Deck app on a laptop, the SDC plugin lets you place buttons and dials from your Lua
config next to ordinary Elgato actions, including the dials and touch strip of a Stream Deck +. The plugin is another
client of the server: it connects to `/plugin/ws` with the same bearer token, subscribes each visible action to an
export, and shows what the server renders. Presses and dial input run your Lua handlers. Page switching, idle and
brightness stay with the Elgato app.

Two actions appear under "Stream Deck Controller": **SDC Button** (keys) and **SDC Dial** (dials, with title, value,
indicator bar and icon).

## Quick start

1. In `config.lua`, export what you want to use ([Exports and dials](configuration.md#exports-and-dials-elgato-plugin)):

   ```lua
   sdc.export("living_light", light("light.living_room", "Living"))
   ```

2. Install `ca.bcaldwell.sdc.streamDeckPlugin` (from a GitHub release, or the `elgato-plugin` artifact of a CI run,
   or `task plugin:pack`) by double-clicking it. Needs the Stream Deck app 7.1 or newer.
3. Drag **SDC Button** or **SDC Dial** onto a key or dial and set the **Server URL** (`wss://your-server/plugin/ws`),
   the **Token** (`SDC_CLIENT_TOKEN`) and the **Export**.

Only exported things are visible to the plugin. The token is stored in the Stream Deck app's preferences as plain
text, so treat the laptop accordingly.

Installation details and building the plugin are in
[plugins/elgato/README.md](../plugins/elgato/README.md); the wire protocol is in
[crates/server/README.md](../crates/server/README.md#plugin-websocket-pluginws).
