// The plugin's single websocket to the server: reconnects with backoff, remembers what every
// visible action subscribed and re-subscribes after a reconnect. Knows nothing about Stream Deck
// (only `ws`), so scripts/smoke.mjs can drive it directly.

import WebSocket from "ws";

import {
	PROTOCOL_VERSION,
	type ClientMessage,
	type DialFeedback,
	type ExportItem,
	type ServerMessage,
	type SlotKind,
	type Subscribe,
} from "./protocol";

/**
 * The plugin endpoint for whatever was typed: a bare host, an https:// URL or the deck's `/ws` all
 * mean `wss://host/plugin/ws`. Other paths are kept (e.g. a server behind a path prefix).
 */
export function pluginUrl(input: string): string {
	if (!input) return input;
	let u: URL;
	try {
		u = new URL(/^[a-z]+:\/\//i.test(input) ? input : `wss://${input}`);
	} catch {
		return input;
	}
	if (u.protocol === "https:") u.protocol = "wss:";
	if (u.protocol === "http:") u.protocol = "ws:";
	const path = u.pathname.replace(/\/+$/, "");
	if (path === "" || path === "/ws") u.pathname = "/plugin/ws";
	return u.toString();
}

const MIN_BACKOFF_MS = 1_000;
const MAX_BACKOFF_MS = 30_000;
const HEARTBEAT_MS = 20_000;
/** Dial ticks are batched this long before they are sent as one message. */
const ROTATE_BATCH_MS = 100;
/** Keys are rendered by the server at this size; Stream Deck scales it to the hardware. */
const KEY_SIZE_PX = 144;

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

export class Hub {
	#log: Logger;
	#clientId: string;
	#url = "";
	#token = "";
	#ws: WebSocket | undefined;
	#online = false;
	#backoff = MIN_BACKOFF_MS;
	#retry: ReturnType<typeof setTimeout> | undefined;
	#heartbeat: ReturnType<typeof setInterval> | undefined;
	#alive = true;
	#slots = new Map<string, { sub: Subscribe; handler: SlotHandler }>();
	#exports: ExportItem[] = [];
	#gotExports!: () => void;
	#exportsReady = new Promise<void>((resolve) => (this.#gotExports = resolve));
	#pendingTicks = new Map<string, number>();
	#rotateTimer: ReturnType<typeof setTimeout> | undefined;

	constructor(log: Logger, clientId: string) {
		this.#log = log;
		this.#clientId = clientId;
	}

	/** Sets the server and token and reconnects if they changed; no URL = stay offline. */
	configure({ url, token }: { url?: string; token?: string }): void {
		const nextUrl = pluginUrl(url?.trim() ?? "");
		const nextToken = token?.trim() ?? "";
		if (nextUrl === this.#url && nextToken === this.#token) return;
		this.#url = nextUrl;
		this.#token = nextToken;
		this.reconnect();
	}

	/** Drops the connection (if any) and connects again right away, e.g. after the computer woke. */
	reconnect(): void {
		clearTimeout(this.#retry);
		this.#backoff = MIN_BACKOFF_MS;
		const old = this.#ws;
		this.#down();
		old?.terminate();
		this.#connect();
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
		if (!this.#online) return false;
		this.#ws?.send(JSON.stringify(msg));
		return true;
	}

	/** Adds dial ticks to the batch for `slot`; false if offline. */
	rotate(slot: string, ticks: number): boolean {
		if (!this.#online) return false;
		this.#pendingTicks.set(slot, (this.#pendingTicks.get(slot) ?? 0) + ticks);
		this.#rotateTimer ??= setTimeout(() => this.#flushTicks(), ROTATE_BATCH_MS);
		return true;
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

	#connect(): void {
		if (!this.#url) return;
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
		ws.on("message", (data) => this.#onMessage(JSON.parse(data.toString()) as ServerMessage));
		ws.on("pong", () => (this.#alive = true));
		ws.on("unexpected-response", (_req, res) => {
			this.#log.warn(`server refused the connection: HTTP ${res.statusCode} (check the token)`);
			res.resume();
		});
		ws.on("error", (e) => this.#log.warn(`websocket error: ${e.message}`));
		ws.on("close", () => {
			if (this.#ws !== ws) return; // replaced by reconnect()
			this.#log.info("disconnected");
			this.#down();
			this.#scheduleRetry();
		});
		this.#heartbeat = setInterval(() => {
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
		const wait = this.#backoff * (0.8 + Math.random() * 0.4);
		this.#backoff = Math.min(this.#backoff * 2, MAX_BACKOFF_MS);
		this.#retry = setTimeout(() => this.#connect(), wait);
	}

	#onMessage(msg: ServerMessage): void {
		switch (msg.type) {
			case "plugin_welcome":
				this.#online = true;
				this.#backoff = MIN_BACKOFF_MS;
				this.#log.info("connected");
				for (const { sub } of this.#slots.values()) this.send(sub);
				break;
			case "exports":
				this.#exports = msg.items;
				this.#gotExports();
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
				this.#log.error(`server error: ${msg.message}`);
				break;
		}
	}
}
