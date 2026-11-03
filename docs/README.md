# Documentation

| Guide | What is in it |
|-------|---------------|
| [Overview](overview.md) | What stream-deck-controller is, how the pieces fit together, a tour of the features |
| [Configuration](configuration.md) | Writing `config.lua`: buttons, render fields, Home Assistant state and services, pages, spans, dials, idle, icons |
| [Raspberry Pi deck](pi.md) | Setting up a Pi + Stream Deck, installing and updating the client, power saving |
| [Browser deck](browser.md) | The deck at `/deck`: own deck, layouts, phones, authentication |
| [Elgato plugin](plugin.md) | Using server-defined buttons and dials in Elgato's Stream Deck app |
| [Deployment](deploy.md) | The Docker image, Kubernetes manifests, environment variables, CI |

Reference material that lives next to the code:

- [crates/server/README.md](../crates/server/README.md): every environment variable, the full Lua API, the plugin
  websocket protocol, image sources.
- [plugins/elgato/README.md](../plugins/elgato/README.md): installing and building the plugin.
- [deploy/pi/README.md](../deploy/pi/README.md): the Pi installer in detail.

The screenshots come from [demo/config.lua](demo/config.lua) with the fake Home Assistant states in
[demo/states.json](demo/states.json); the guide in [overview.md](overview.md#regenerating-the-screenshots) shows how
to regenerate them.
