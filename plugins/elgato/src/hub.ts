// The plugin's single websocket to the server: reconnects with backoff, remembers what every
// visible action subscribed and re-subscribes after a reconnect.

import streamDeck from "@elgato/streamdeck";

import { Connection } from "./conn";
import {
	KEY_SIZE_PX,
	PROTOCOL_VERSION,
	type ClientMessage,
	type DialFeedback,
	type ExportItem,
	type ServerMessage,
	type SlotKind,
	type Subscribe,
} from "./protocol";

/** Dial ticks are batched this long before they are sent as one message. */
const ROTATE_BATCH_MS = 100;

/** What a subscribed slot (an action instance) receives. */
export interface SlotHandler {
	keyImage?(pngBase64: string): void;
	dialFeedback?(feedback: DialFeedback): void;
	error?(message: string): void;
}

export class Hub {
	#clientId: string;
	/** Called whenever the server sends an export list. */
	onExports: (() => void) | undefined;
	#conn: Connection;
	#online = false;
	#slots = new Map<string, { sub: Subscribe; handler: SlotHandler }>();
	#exports: ExportItem[] = [];
	#gotExports!: () => void;
	#exportsReady = new Promise<void>((resolve) => (this.#gotExports = resolve));
	#pendingTicks = new Map<string, number>();
	#rotateTimer: ReturnType<typeof setTimeout> | undefined;

	constructor(clientId: string) {
		this.#clientId = clientId;
		this.#conn = new Connection("plugin", {
			open: () =>
				this.#conn.send({ type: "plugin_hello", protocol_version: PROTOCOL_VERSION, client_id: this.#clientId }),
			message: (data) => this.#onMessage(JSON.parse(data.toString()) as ServerMessage),
			down: () => (this.#online = false),
		});
	}

	/** Sets the server and token and reconnects if they changed; no URL = stay offline. */
	configure(settings: { url?: string; token?: string }): void {
		this.#conn.configure(settings);
	}

	/** Drops the connection (if any) and connects again right away, e.g. after the computer woke. */
	reconnect(): void {
		this.#conn.reconnect();
	}

	/** Shows export `exportId` in `slot`; resent automatically after reconnects. */
	subscribe(slot: string, exportId: string, kind: SlotKind, handler: SlotHandler): void {
		const sub: Subscribe = { type: "subscribe", slot, export: exportId, kind, size_px: KEY_SIZE_PX };
		this.#slots.set(slot, { sub, handler });
		this.send(sub);
	}

	unsubscribe(slot: string): void {
		if (this.#slots.delete(slot)) {
			this.#pendingTicks.delete(slot);
			this.send({ type: "unsubscribe", slot });
		}
	}

	/** Sends `msg` if online; false if it was dropped (the caller shows an alert). */
	send(msg: ClientMessage): boolean {
		return this.#online && this.#conn.send(msg);
	}

	/** Adds dial ticks to the batch for `slot`; false if offline. */
	rotate(slot: string, ticks: number): boolean {
		if (!this.#online) return false;
		this.#pendingTicks.set(slot, (this.#pendingTicks.get(slot) ?? 0) + ticks);
		this.#rotateTimer ??= setTimeout(() => this.#flushTicks(), ROTATE_BATCH_MS);
		return true;
	}

	get exports(): readonly ExportItem[] {
		return this.#exports;
	}

	/** The server's export list: waits up to `timeoutMs` for the first one, empty if it never came. */
	async waitForExports(timeoutMs: number): Promise<readonly ExportItem[]> {
		await Promise.race([this.#exportsReady, new Promise((resolve) => setTimeout(resolve, timeoutMs))]);
		return this.#exports;
	}

	#flushTicks(): void {
		this.#rotateTimer = undefined;
		const batch = [...this.#pendingTicks];
		this.#pendingTicks.clear();
		for (const [slot, ticks] of batch) {
			if (ticks !== 0) this.send({ type: "dial_rotate", slot, ticks });
		}
	}

	#onMessage(msg: ServerMessage): void {
		switch (msg.type) {
			case "plugin_welcome":
				this.#online = true;
				this.#conn.connected();
				streamDeck.logger.info("connected");
				for (const { sub } of this.#slots.values()) this.send(sub);
				break;
			case "exports":
				this.#exports = msg.items;
				this.#gotExports();
				this.onExports?.();
				break;
			case "key_image":
				this.#slots.get(msg.slot)?.handler.keyImage?.(msg.png_base64);
				break;
			case "dial_feedback":
				this.#slots.get(msg.slot)?.handler.dialFeedback?.(msg);
				break;
			case "slot_error":
				this.#slots.get(msg.slot)?.handler.error?.(msg.message);
				break;
			case "error":
				streamDeck.logger.error(`server error: ${msg.message}`);
				break;
		}
	}
}
