// One reconnecting websocket to the server, shared by the plugin hub (/plugin/ws) and the
// per-device deck links (/ws): backoff, heartbeat, the bearer token. What is spoken over it is
// up to the owner.

import streamDeck from "@elgato/streamdeck";
import WebSocket from "ws";

/** The two routes of the server a plugin talks to. */
export type Endpoint = "plugin" | "deck";

const PATH: Record<Endpoint, string> = { plugin: "/plugin/ws", deck: "/ws" };

/**
 * The websocket URL for whatever was typed: a bare host, an https:// URL, the deck's `/ws` or
 * the plugin's `/plugin/ws` all mean `wss://host<endpoint path>`. Other paths are kept (e.g. a
 * server behind a path prefix) except a trailing `/plugin/ws` or `/ws`, which is swapped.
 */
export function serverUrl(input: string, endpoint: Endpoint): string {
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
	const prefix = path.replace(/\/(plugin\/)?ws$/, "");
	if (prefix !== path || path === "") u.pathname = prefix + PATH[endpoint];
	return u.toString();
}

const MIN_BACKOFF_MS = 1_000;
const MAX_BACKOFF_MS = 30_000;
const HEARTBEAT_MS = 20_000;

export interface ConnectionEvents {
	/** The socket opened: send the hello. */
	open(): void;
	message(data: Buffer, binary: boolean): void;
	/** The socket is gone (a retry is scheduled unless the connection was closed). */
	down(): void;
}

export class Connection {
	readonly #endpoint: Endpoint;
	readonly #events: ConnectionEvents;
	#url = "";
	#token = "";
	#ws: WebSocket | undefined;
	#backoff = MIN_BACKOFF_MS;
	#retry: ReturnType<typeof setTimeout> | undefined;
	#heartbeat: ReturnType<typeof setInterval> | undefined;
	#alive = true;

	constructor(endpoint: Endpoint, events: ConnectionEvents) {
		this.#endpoint = endpoint;
		this.#events = events;
	}

	/** Sets the server and token and reconnects if they changed; no URL = stay offline. */
	configure({ url, token }: { url?: string; token?: string }): void {
		const nextUrl = serverUrl(url?.trim() ?? "", this.#endpoint);
		const nextToken = token?.trim() ?? "";
		if (nextUrl === this.#url && nextToken === this.#token) return;
		this.#url = nextUrl;
		this.#token = nextToken;
		this.reconnect();
	}

	/** Drops the connection (if any) and connects again right away, e.g. after the computer woke. */
	reconnect(): void {
		this.close();
		this.#connect();
	}

	/** Drops the connection and stops retrying until `reconnect()`. */
	close(): void {
		clearTimeout(this.#retry);
		this.#backoff = MIN_BACKOFF_MS;
		const old = this.#ws;
		this.#down();
		old?.terminate();
	}

	/** The server answered the hello: the next failure starts the backoff over. */
	connected(): void {
		this.#backoff = MIN_BACKOFF_MS;
	}

	/** Sends `msg` as JSON; false if there is no open socket. */
	send(msg: object): boolean {
		if (this.#ws?.readyState !== WebSocket.OPEN) return false;
		this.#ws.send(JSON.stringify(msg));
		return true;
	}

	#connect(): void {
		if (!this.#url) return;
		const headers: Record<string, string> = {};
		if (this.#token) headers["Authorization"] = `Bearer ${this.#token}`;
		let ws: WebSocket;
		try {
			ws = new WebSocket(this.#url, { headers, handshakeTimeout: 10_000 });
		} catch (e) {
			streamDeck.logger.error(`cannot connect to ${this.#url}: ${e}`);
			this.#scheduleRetry();
			return;
		}
		this.#ws = ws;
		streamDeck.logger.info(`connecting to ${this.#url}`);
		ws.on("open", () => {
			this.#alive = true;
			this.#events.open();
		});
		ws.on("message", (data, binary) => this.#events.message(data as Buffer, binary));
		ws.on("pong", () => (this.#alive = true));
		ws.on("unexpected-response", (_req, res) => {
			streamDeck.logger.warn(`server refused the connection: HTTP ${res.statusCode} (check the token)`);
			res.resume();
		});
		ws.on("error", (e) => streamDeck.logger.warn(`websocket error: ${e.message}`));
		ws.on("close", () => {
			if (this.#ws !== ws) return; // replaced by reconnect()
			streamDeck.logger.info("disconnected");
			this.#down();
			this.#scheduleRetry();
		});
		this.#heartbeat = setInterval(() => {
			if (!this.#alive) {
				streamDeck.logger.warn("no answer to ping, dropping the connection");
				ws.terminate();
				return;
			}
			this.#alive = false;
			ws.ping();
		}, HEARTBEAT_MS);
	}

	#down(): void {
		this.#ws = undefined;
		clearInterval(this.#heartbeat);
		this.#events.down();
	}

	#scheduleRetry(): void {
		const wait = this.#backoff * (0.8 + Math.random() * 0.4);
		this.#backoff = Math.min(this.#backoff * 2, MAX_BACKOFF_MS);
		this.#retry = setTimeout(() => this.#connect(), wait);
	}
}
