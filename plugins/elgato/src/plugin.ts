import { hostname } from "node:os";

import streamDeck from "@elgato/streamdeck";

import { SdcButton } from "./actions/button";
import { SdcDial } from "./actions/dial";
import { sendExports } from "./actions/shared";
import { Hub } from "./hub";

/** Global settings, written by the property inspector (shared by all actions). */
type GlobalSettings = {
	url?: string;
	token?: string;
};

const hub = new Hub(`elgato-${hostname()}`);

streamDeck.actions.registerAction(new SdcButton(hub));
streamDeck.actions.registerAction(new SdcDial(hub));

// An inspector opened before the first export list arrived refreshes its dropdown now.
hub.onExports = () => {
	const open = streamDeck.ui.action;
	if (open) void sendExports(hub, open.manifestId === "ca.bcaldwell.sdc.dial" ? "dial" : "key");
};

streamDeck.settings.onDidReceiveGlobalSettings<GlobalSettings>((ev) => hub.configure(ev.settings));
// Wake from sleep: the old socket is usually dead, do not wait for the heartbeat to notice.
streamDeck.system.onSystemDidWakeUp(() => hub.reconnect());

await streamDeck.connect();
hub.configure(await streamDeck.settings.getGlobalSettings<GlobalSettings>());
