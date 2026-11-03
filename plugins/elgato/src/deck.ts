// Deck mode: for every physical Stream Deck that shows at least one SDC Deck Key, the plugin is
// a normal deck client (the same protocol as sdc-client on a Pi) on the server's `/ws`. The
// server runs the whole page set (navigation, spans, press flash); each key of the physical
// deck shows the image of the same index, and keys without an SDC Deck Key are simply not shown.

import streamDeck, { DeviceType, type ActionContext, type Device, type KeyAction } from "@elgato/streamdeck";
import type { JsonObject } from "@elgato/utils";

import { Connection } from "./conn";
import { KEY_SIZE_PX, PROTOCOL_VERSION, type DeckClientMessage, type DeckServerMessage } from "./protocol";

export type DeckSettings = {
	url?: string;
	token?: string;
	/** The `sdc.device` name to request; empty = the server's default for the deck type. */
	layout?: string;
};

/** The `elgato-streamdeck` kind names the server derives its device type from (`device_type`). */
const MODEL: Partial<Record<DeviceType, string>> = {
	[DeviceType.StreamDeck]: "Mk2",
	[DeviceType.StreamDeckMini]: "Mini",
	[DeviceType.StreamDeckXL]: "Xl",
	[DeviceType.StreamDeckPedal]: "Pedal",
	[DeviceType.StreamDeckPlus]: "Plus",
	[DeviceType.StreamDeckNeo]: "Neo",
	[DeviceType.StreamDeckPlusXL]: "PlusXl",
};

type Key = KeyAction<JsonObject>;

const layoutOf = (settings: DeckSettings): string | undefined => settings.layout?.trim() || undefined;

/** Key index on the deck, row by row like the Pi's decks; undefined inside a multi-action. */
const keyIndex = (key: Key): number | undefined => {
	const at = key.coordinates;
	return at && at.row * key.device.size.columns + at.column;
};

/** One deck connection for one physical Stream Deck. */
class DeckLink {
	readonly #device: Device;
	readonly #clientId: string;
	readonly #conn: Connection;
	readonly #keys = new Map<number, Key>();
	/** The last image per key: a key that appears later (profile switch) shows it right away. */
	/** Key index by action id, to find a disappearing action again. */
	readonly #indexes = new Map<string, number>();
	readonly #images = new Map<number, string>();
	#layout: string | undefined;
	#online = false;

	constructor(device: Device, clientId: string, settings: DeckSettings) {
		this.#device = device;
		this.#clientId = clientId;
		this.#layout = layoutOf(settings);
		this.#conn = new Connection("deck", {
			open: () => this.#hello(),
			message: (data, binary) => (binary ? this.#onImage(data) : this.#onText(data.toString())),
			down: () => (this.#online = false),
		});
		this.#conn.configure(settings);
	}

	configure(settings: DeckSettings): void {
		const layout = layoutOf(settings);
		const layoutChanged = layout !== this.#layout;
		this.#layout = layout;
		this.#conn.configure(settings);
		// the layout is part of the hello, so a changed one needs a fresh connection
		if (layoutChanged) this.#conn.reconnect();
	}

	reconnect(): void {
		this.#conn.reconnect();
	}

	close(): void {
		this.#conn.close();
	}

	get empty(): boolean {
		return this.#keys.size === 0;
	}

	add(key: Key): void {
		const index = keyIndex(key);
		if (index === undefined) return;
		this.#keys.set(index, key);
		this.#indexes.set(key.id, index);
		const image = this.#images.get(index);
		if (image) void key.setImage(image);
	}

	remove(action: ActionContext): void {
		const index = this.#indexes.get(action.id);
		if (index === undefined) return;
		this.#indexes.delete(action.id);
		this.#keys.delete(index);
	}

	/** Sends a press or release; false if offline (the caller shows an alert). */
	press(key: Key, down: boolean): boolean {
		const index = keyIndex(key);
		if (index === undefined || !this.#online) return false;
		return this.#conn.send({ type: down ? "key_down" : "key_up", key: index } satisfies DeckClientMessage);
	}

	#hello(): void {
		const { rows, columns } = this.#device.size;
		const hello: DeckClientMessage = {
			type: "hello",
			protocol_version: PROTOCOL_VERSION,
			client_id: this.#clientId,
			device: {
				model: MODEL[this.#device.type] ?? this.#device.name,
				serial: null,
				rows,
				cols: columns,
				key_size_px: KEY_SIZE_PX,
			},
			device_name: this.#layout,
		};
		this.#conn.send(hello);
	}

	#onText(text: string): void {
		const msg = JSON.parse(text) as DeckServerMessage;
		switch (msg.type) {
			case "welcome":
				this.#online = true;
				this.#conn.connected();
				streamDeck.logger.info(`deck ${this.#clientId} connected`);
				break;
			case "error":
				streamDeck.logger.error(`server error for ${this.#clientId}: ${msg.message}`);
				break;
			// brightness is the Elgato app's; client updates are for sdc-client, not the plugin
		}
	}

	#onImage(frame: Buffer): void {
		const index = frame[0]!;
		const url = `data:image/jpeg;base64,${frame.subarray(1).toString("base64")}`;
		this.#images.set(index, url);
		void this.#keys.get(index)?.setImage(url);
	}
}

/** The deck links of all connected Stream Decks, alive while they show SDC Deck Keys. */
export class Decks {
	readonly #hostId: string;
	readonly #links = new Map<string, DeckLink>();
	#settings: DeckSettings = {};

	constructor(hostId: string) {
		this.#hostId = hostId;
	}

	configure(settings: DeckSettings): void {
		this.#settings = settings;
		for (const link of this.#links.values()) link.configure(settings);
	}

	reconnect(): void {
		for (const link of this.#links.values()) link.reconnect();
	}

	add(key: Key): void {
		const id = key.device.id;
		let link = this.#links.get(id);
		if (!link) {
			link = new DeckLink(key.device, `${this.#hostId}-${id}`, this.#settings);
			this.#links.set(id, link);
		}
		link.add(key);
	}

	remove(action: ActionContext): void {
		const id = action.device.id;
		const link = this.#links.get(id);
		link?.remove(action);
		if (link?.empty) {
			link.close();
			this.#links.delete(id);
		}
	}

	/** Sends a press or release of `key`; false if offline (the caller shows an alert). */
	press(key: Key, down: boolean): boolean {
		return this.#links.get(key.device.id)?.press(key, down) ?? false;
	}
}
