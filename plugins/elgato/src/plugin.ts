import { hostname } from "node:os";

import streamDeck from "@elgato/streamdeck";

import { SdcButton } from "./actions/button";
import { SdcDial } from "./actions/dial";
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

streamDeck.settings.onDidReceiveGlobalSettings<GlobalSettings>((ev) => hub.configure(ev.settings));
// Wake from sleep: the old socket is usually dead, do not wait for the heartbeat to notice.
streamDeck.system.onSystemDidWakeUp(() => hub.reconnect());

await streamDeck.connect();
hub.configure(await streamDeck.settings.getGlobalSettings<GlobalSettings>());
