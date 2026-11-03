import { hostname } from "node:os";

import streamDeck from "@elgato/streamdeck";

import { SdcButton } from "./actions/button";
import { SdcDeckKey } from "./actions/deck-key";
import { SdcDial } from "./actions/dial";
import { sendExports, sendStatus } from "./actions/shared";
import { statusChanged } from "./conn";
import { Decks } from "./deck";
import { Hub } from "./hub";

/** Global settings, written by the property inspector (shared by all actions). */
type GlobalSettings = {
	url?: string;
	token?: string;
	/** The `sdc.device` name SDC Deck Keys request. */
	layout?: string;
};

const clientId = `elgato-${hostname()}`;
const hub = new Hub(clientId);
const decks = new Decks(clientId);

const configure = (settings: GlobalSettings): void => {
	hub.configure(settings);
	decks.configure(settings);
};

streamDeck.actions.registerAction(new SdcButton(hub));
streamDeck.actions.registerAction(new SdcDial(hub));
streamDeck.actions.registerAction(new SdcDeckKey(decks));

// An inspector opened before the first export list arrived refreshes its dropdown now.
hub.onExports = () => {
	const open = streamDeck.ui.action;
	if (open?.manifestId === "ca.bcaldwell.sdc.dial") void sendExports(hub, "dial");
	else if (open?.manifestId === "ca.bcaldwell.sdc.button") void sendExports(hub, "key");
};

// The inspector's Connection section follows the connection live.
const pushStatus = (): void => void (streamDeck.ui.action && sendStatus());
statusChanged.listener = pushStatus;
streamDeck.ui.onDidAppear(pushStatus);

streamDeck.settings.onDidReceiveGlobalSettings<GlobalSettings>((ev) => configure(ev.settings));
// Wake from sleep: the old socket is usually dead, do not wait for the heartbeat to notice.
streamDeck.system.onSystemDidWakeUp(() => {
	hub.reconnect();
	decks.reconnect();
});

await streamDeck.connect();
configure(await streamDeck.settings.getGlobalSettings<GlobalSettings>());
