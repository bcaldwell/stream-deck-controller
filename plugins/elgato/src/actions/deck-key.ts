import {
	action,
	SingletonAction,
	type KeyDownEvent,
	type KeyUpEvent,
	type WillAppearEvent,
	type WillDisappearEvent,
} from "@elgato/streamdeck";

import type { Decks } from "../deck";

/**
 * A key of a whole server page set: shows the key at its own position on the Stream Deck and
 * presses it. Place one on every key you want the server to drive; it has no settings of its own.
 */
@action({ UUID: "ca.bcaldwell.sdc.deckkey" })
export class SdcDeckKey extends SingletonAction {
	readonly #decks: Decks;

	constructor(decks: Decks) {
		super();
		this.#decks = decks;
	}

	override onWillAppear(ev: WillAppearEvent): void {
		// the manifest limits this action to the keypad
		if (ev.action.isKey()) this.#decks.add(ev.action);
	}

	override onWillDisappear(ev: WillDisappearEvent): void {
		this.#decks.remove(ev.action);
	}

	override async onKeyDown(ev: KeyDownEvent): Promise<void> {
		if (!this.#decks.press(ev.action, true)) await ev.action.showAlert();
	}

	override onKeyUp(ev: KeyUpEvent): void {
		this.#decks.press(ev.action, false);
	}
}
