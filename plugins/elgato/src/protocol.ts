// The /plugin/ws messages, mirroring crates/protocol/src/lib.rs.

export const PROTOCOL_VERSION = 1;

/** Keys are rendered by the server at this size; Stream Deck scales it to the hardware. */
export const KEY_SIZE_PX = 144;

export type SlotKind = "key" | "dial";

export interface ExportItem {
	id: string;
	kind: SlotKind;
	label?: string;
}

export interface Subscribe {
	type: "subscribe";
	slot: string;
	export: string;
	kind: SlotKind;
	size_px: number;
}

/** Plugin -> server. */
export type ClientMessage =
	| { type: "plugin_hello"; protocol_version: number; client_id: string }
	| Subscribe
	| { type: "unsubscribe"; slot: string }
	| { type: "slot_down"; slot: string }
	| { type: "slot_up"; slot: string }
	| { type: "dial_rotate"; slot: string; ticks: number }
	| { type: "dial_down"; slot: string }
	| { type: "dial_up"; slot: string }
	| { type: "touch_tap"; slot: string }
	| { type: "list_exports" };

export interface DialFeedback {
	type: "dial_feedback";
	slot: string;
	title: string;
	value: string;
	/** 0-100 */
	indicator: number;
	icon_png_base64?: string;
}

/** Server -> plugin. */
export type ServerMessage =
	| { type: "plugin_welcome"; protocol_version: number }
	| { type: "exports"; items: ExportItem[] }
	| { type: "key_image"; slot: string; png_base64: string }
	| DialFeedback
	| { type: "slot_error"; slot: string; message: string }
	| { type: "error"; message: string };

/** Deck -> server (`/ws`), the subset a Stream Deck app deck sends. */
export type DeckClientMessage =
	| {
			type: "hello";
			protocol_version: number;
			client_id: string;
			device: { model: string; serial: null; rows: number; cols: number; key_size_px: number };
			device_name?: string;
	  }
	| { type: "key_down"; key: number }
	| { type: "key_up"; key: number };

/** Server -> deck (`/ws`) text frames; key images arrive as binary `[key u8][jpeg]`. */
export type DeckServerMessage =
	| { type: "welcome"; protocol_version: number }
	| { type: "brightness"; percent: number }
	| { type: "error"; message: string }
	| { type: "update"; path: string; sha256: string };
