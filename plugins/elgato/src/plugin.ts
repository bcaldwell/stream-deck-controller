import { hostname } from "node:os";

import streamDeck from "@elgato/streamdeck";

import { SdcButton } from "./actions/button";
import { SdcDial } from "./actions/dial";
import { pushExports } from "./actions/shared";
import { Hub } from "./hub";

/** Global settings, written by the property inspector (shared by all actions). */
type GlobalSettings = {
	url?: string;
	token?: string;
};

const log = {
	info: (...a: unknown[]) => streamDeck.logger.info(...a),
	warn: (...a: unknown[]) => streamDeck.logger.warn(...a),
	error: (...a: unknown[]) => streamDeck.logger.error(...a),
};

const hub = new Hub(log, `elgato-${hostname()}`);

streamDeck.actions.registerAction(new SdcButton(hub));
streamDeck.actions.registerAction(new SdcDial(hub));

// An open property inspector shows the current list; its dropdown refreshes when we push one.
hub.onExports(() => {
	pushExports(hub, streamDeck.ui.action?.manifestId === "ca.bcaldwell.sdc.dial" ? "dial" : "key");
});

streamDeck.settings.onDidReceiveGlobalSettings<GlobalSettings>((ev) => hub.configure(ev.settings));
// Wake from sleep: the old socket is usually dead, do not wait for the heartbeat to notice.
streamDeck.system.onSystemDidWakeUp(() => hub.reconnect());

await streamDeck.connect();
hub.configure(await streamDeck.settings.getGlobalSettings<GlobalSettings>());
