// Headless check of the plugin's connection code against a running server:
//   docker run --rm -p 7700:7700 -e SDC_CLIENT_TOKEN=test \
//     -v "$PWD/scripts/smoke-config.lua:/config/config.lua:ro" -v "$PWD/../../icons:/config/icons:ro" stream-deck-controller:dev
//   node scripts/smoke.mjs [ws://127.0.0.1:7700/plugin/ws] [token]
// Subscribes a key and a dial through the plugin's Hub, prints what arrives and writes the key's
// PNG to $TMPDIR. Run with node >= 22.18 (types are stripped, no build needed).

import { writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { Hub } from "../src/hub.ts";

const url = process.argv[2] ?? "ws://127.0.0.1:7700/plugin/ws";
const token = process.argv[3] ?? process.env.SDC_CLIENT_TOKEN ?? "test";
const log = { info: (...a) => console.log("[hub]", ...a), warn: (...a) => console.log("[hub warn]", ...a), error: (...a) => console.log("[hub error]", ...a) };

const hub = new Hub(log, "smoke");
const got = { key: undefined, dial: undefined, errors: [] };
const done = new Promise((resolve) => {
	const check = () => got.key && got.dial && resolve();
	hub.configure({ url, token });
	hub.waitForExports(5000).then((items) => {
		console.log("exports:", JSON.stringify(items));
		hub.subscribe("key-1", "lamp", "key", {
			keyImage: (b64) => ((got.key = b64), check()),
			error: (m) => got.errors.push(m),
		});
		hub.subscribe("dial-1", "dimmer", "dial", {
			dialFeedback: (f) => ((got.dial = f), check()),
			error: (m) => got.errors.push(m),
		});
		hub.subscribe("bad-1", "does-not-exist", "key", { error: (m) => got.errors.push(m) });
	});
});

const timeout = setTimeout(() => {
	console.error("timed out; got =", Object.keys(got).filter((k) => got[k]));
	process.exit(1);
}, 10_000);
await done;
clearTimeout(timeout);

const png = join(tmpdir(), "sdc-smoke-key.png");
writeFileSync(png, Buffer.from(got.key, "base64"));
console.log("key image written to", png);
const { icon_png_base64, ...dial } = got.dial;
console.log("dial feedback:", JSON.stringify(dial), "icon:", icon_png_base64 ? `${icon_png_base64.length} base64 chars` : "none");
if (icon_png_base64) {
	const iconPath = join(tmpdir(), "sdc-smoke-dial-icon.png");
	writeFileSync(iconPath, Buffer.from(icon_png_base64, "base64"));
	console.log("dial icon written to", iconPath);
}

// input: three quick ticks must arrive as one rotate(ctx, 3); the server logs it
hub.send({ type: "slot_down", slot: "key-1" });
hub.send({ type: "slot_up", slot: "key-1" });
hub.rotate("dial-1", 1);
hub.rotate("dial-1", 1);
hub.rotate("dial-1", 1);
hub.send({ type: "dial_down", slot: "dial-1" });
await new Promise((r) => setTimeout(r, 500));
console.log("slot errors:", got.errors);
process.exit(got.errors.length === 1 ? 0 : 1);
