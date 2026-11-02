// The /plugin/ws messages, mirroring crates/protocol/src/lib.rs. Types only (erased at build
// time) so that hub.ts can run unbundled under node's type stripping in scripts/smoke.mjs.

export const PROTOCOL_VERSION = 1;

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
