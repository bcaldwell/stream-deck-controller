// The plugin's single websocket to the server: reconnects with backoff, remembers what every
// visible action subscribed and re-subscribes after a reconnect. Knows nothing about Stream Deck
// (only `ws`), so scripts/smoke.mjs can drive it directly.

import WebSocket from "ws";

import type { ClientMessage, DialFeedback, ExportItem, ServerMessage, SlotKind, Subscribe } from "./protocol";

export const DEFAULT_URL = "wss://your-server/plugin/ws";
const PROTOCOL_VERSION = 1;
const MIN_BACKOFF_MS = 1_000;
const MAX_BACKOFF_MS = 30_000;
const HEARTBEAT_MS = 20_000;
/** Dial ticks are batched this long before they are sent as one message. */
const ROTATE_BATCH_MS = 100;
/** Keys are rendered by the server at this size; Stream Deck scales it to the hardware. */
export const KEY_SIZE_PX = 144;

/** What a subscribed slot (an action instance) receives. */
export interface SlotHandler {
	keyImage?(pngBase64: string): void;
	dialFeedback?(feedback: DialFeedback): void;
	error?(message: string): void;
}

export interface Logger {
	info(...args: unknown[]): void;
	warn(...args: unknown[]): void;
	error(...args: unknown[]): void;
}

export interface Connection {
	url?: string;
	token?: string;
}

export class Hub {
	#log: Logger;
	#clientId: string;
	#url = DEFAULT_URL;
	#token = "";
	#ws: WebSocket | undefined;
	#online = false;
	#started = false;
	#backoff = MIN_BACKOFF_MS;
	#retry: ReturnType<typeof setTimeout> | undefined;
	#heartbeat: ReturnType<typeof setInterval> | undefined;
	#alive = true;
	#subs = new Map<string, Subscribe>();
	#handlers = new Map<string, SlotHandler>();
	#exports: ExportItem[] = [];
	#exportsKnown = false;
	#exportListeners = new Set<() => void>();
	#pendingTicks = new Map<string, number>();
	#rotateTimer: ReturnType<typeof setTimeout> | undefined;

	constructor(log: Logger, clientId: string) {
		this.#log = log;
		this.#clientId = clientId;
	}

	/** Whether the server accepted us (welcome received) and the socket is still open. */
	get online(): boolean {
		return this.#online;
	}

	/** The last export list the server sent (empty until the first connect). */
	get exports(): readonly ExportItem[] {
		return this.#exports;
	}

	/** Sets the server and token (empty = defaults) and (re)connects if they changed. */
	configure({ url, token }: Connection): void {
		const nextUrl = url?.trim() || DEFAULT_URL;
		const nextToken = token?.trim() ?? "";
		const changed = nextUrl !== this.#url || nextToken !== this.#token;
		this.#url = nextUrl;
		this.#token = nextToken;
		if (!this.#started) {
			this.#started = true;
			this.#connect();
		} else if (changed) {
			this.reconnect();
		}
	}

	/** Drops the connection (if any) and connects again right away, e.g. after the computer woke. */
	reconnect(): void {
		clearTimeout(this.#retry);
		this.#backoff = MIN_BACKOFF_MS;
		this.#ws?.removeAllListeners();
		this.#ws?.terminate();
		this.#down();
		this.#connect();
	}

	close(): void {
		this.#started = false;
		clearTimeout(this.#retry);
		clearTimeout(this.#rotateTimer);
		this.#ws?.removeAllListeners();
		this.#ws?.close();
		this.#down();
	}

	/** Shows export `exportId` in `slot`; resent automatically after reconnects. */
	subscribe(slot: string, exportId: string, kind: SlotKind, handler: SlotHandler): void {
		const sub: Subscribe = { type: "subscribe", slot, export: exportId, kind, size_px: KEY_SIZE_PX };
		this.#subs.set(slot, sub);
		this.#handlers.set(slot, handler);
		this.send(sub);
	}

	unsubscribe(slot: string): void {
		if (this.#subs.delete(slot)) {
			this.#handlers.delete(slot);
			this.#pendingTicks.delete(slot);
			this.send({ type: "unsubscribe", slot });
		}
	}

	/** Sends `msg` if online; false if it was dropped (the caller shows an alert). */
	send(msg: ClientMessage): boolean {
		if (!this.#online || this.#ws?.readyState !== WebSocket.OPEN) return false;
		this.#ws.send(JSON.stringify(msg));
		return true;
	}

	/** Adds dial ticks to the batch for `slot`; false if offline. */
	rotate(slot: string, ticks: number): boolean {
		if (!this.#online) return false;
		this.#pendingTicks.set(slot, (this.#pendingTicks.get(slot) ?? 0) + ticks);
		this.#rotateTimer ??= setTimeout(() => this.#flushTicks(), ROTATE_BATCH_MS);
		return true;
	}

	/** Resolves with the export list once the server has sent one, or after `timeoutMs`. */
	async waitForExports(timeoutMs: number): Promise<readonly ExportItem[]> {
		if (this.#exportsKnown) return this.#exports;
		await new Promise<void>((resolve) => {
			const done = (): void => {
				clearTimeout(timer);
				this.#exportListeners.delete(done);
				resolve();
			};
			const timer = setTimeout(done, timeoutMs);
			this.#exportListeners.add(done);
		});
		return this.#exports;
	}

	/** Calls `listener` whenever the server sends an export list. */
	onExports(listener: () => void): void {
		this.#exportListeners.add(listener);
	}

	#flushTicks(): void {
		this.#rotateTimer = undefined;
		const batch = [...this.#pendingTicks];
		this.#pendingTicks.clear();
		for (const [slot, ticks] of batch) {
			if (ticks !== 0) this.send({ type: "dial_rotate", slot, ticks });
		}
	}

	#connect(): void {
		if (!this.#started) return;
		const headers: Record<string, string> = {};
		if (this.#token) headers["Authorization"] = `Bearer ${this.#token}`;
		let ws: WebSocket;
		try {
			ws = new WebSocket(this.#url, { headers, handshakeTimeout: 10_000 });
		} catch (e) {
			this.#log.error(`cannot connect to ${this.#url}: ${e}`);
			this.#scheduleRetry();
			return;
		}
		this.#ws = ws;
		this.#log.info(`connecting to ${this.#url}`);
		ws.on("open", () => {
			this.#alive = true;
			ws.send(
				JSON.stringify({ type: "plugin_hello", protocol_version: PROTOCOL_VERSION, client_id: this.#clientId }),
			);
		});
		ws.on("message", (data) => this.#onMessage(data.toString()));
		ws.on("pong", () => (this.#alive = true));
		ws.on("unexpected-response", (_req, res) => {
			this.#log.warn(`server refused the connection: HTTP ${res.statusCode} (check the token)`);
			res.resume();
		});
		ws.on("error", (e) => this.#log.warn(`websocket error: ${e.message}`));
		ws.on("close", () => {
			if (this.#ws !== ws) return;
			this.#log.info("disconnected");
			this.#down();
			this.#scheduleRetry();
		});
		clearInterval(this.#heartbeat);
		this.#heartbeat = setInterval(() => {
			if (this.#ws !== ws) return;
			if (!this.#alive) {
				this.#log.warn("no answer to ping, dropping the connection");
				ws.terminate();
				return;
			}
			this.#alive = false;
			ws.ping();
		}, HEARTBEAT_MS);
	}

	#down(): void {
		this.#online = false;
		this.#ws = undefined;
		clearInterval(this.#heartbeat);
	}

	#scheduleRetry(): void {
		if (!this.#started) return;
		clearTimeout(this.#retry);
		const wait = this.#backoff * (0.8 + Math.random() * 0.4);
		this.#backoff = Math.min(this.#backoff * 2, MAX_BACKOFF_MS);
		this.#retry = setTimeout(() => this.#connect(), wait);
	}

	#onMessage(text: string): void {
		let msg: ServerMessage;
		try {
			msg = JSON.parse(text) as ServerMessage;
		} catch {
			this.#log.warn("unparseable message from the server");
			return;
		}
		switch (msg.type) {
			case "plugin_welcome":
				this.#online = true;
				this.#backoff = MIN_BACKOFF_MS;
				this.#log.info("connected");
				for (const sub of this.#subs.values()) this.send(sub);
				break;
			case "exports":
				this.#exports = msg.items;
				this.#exportsKnown = true;
				for (const l of [...this.#exportListeners]) l();
				break;
			case "key_image":
				this.#handlers.get(msg.slot)?.keyImage?.(msg.png_base64);
				break;
			case "dial_feedback":
				this.#handlers.get(msg.slot)?.dialFeedback?.(msg);
				break;
			case "slot_error":
				this.#handlers.get(msg.slot)?.error?.(msg.message);
				break;
			case "error":
				this.#log.error(`server error: ${msg.message}`);
				break;
		}
	}
}
