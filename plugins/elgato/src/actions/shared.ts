import { streamDeck, type SendToPluginEvent } from "@elgato/streamdeck";
import type { JsonObject, JsonValue } from "@elgato/utils";

import type { Hub } from "../hub";
import type { ExportItem, SlotKind } from "../protocol";

/** Per-action settings, written by the property inspector. */
export type ExportSettings = {
	/** Picked from the server's list. */
	export?: string;
	/** Typed by hand; wins over `export` (for when the list is unavailable). */
	exportManual?: string;
};

/** The export id an action instance shows, if any. */
export const exportId = (s: ExportSettings): string | undefined => s.exportManual?.trim() || s.export?.trim() || undefined;

/** How long the property inspector's dropdown waits for the server's list. */
const EXPORTS_WAIT_MS = 4_000;

const itemsOf = (exports: readonly ExportItem[], kind: SlotKind) =>
	exports
		.filter((e) => e.kind === kind)
		.map((e) => ({ label: e.label ? `${e.label} (${e.id})` : e.id, value: e.id }));

/** Answers the property inspector's `getExports` data source request with the exports of `kind`. */
export async function answerExports(hub: Hub, ev: SendToPluginEvent<JsonValue, JsonObject>, kind: SlotKind): Promise<void> {
	const payload = ev.payload;
	if (typeof payload !== "object" || payload === null || Array.isArray(payload) || payload["event"] !== "getExports") return;
	const exports = await hub.waitForExports(EXPORTS_WAIT_MS);
	await streamDeck.ui.sendToPropertyInspector({ event: "getExports", items: itemsOf(exports, kind) });
}

/** Re-sends the list to an open property inspector (the dropdown has `hot-reload`). */
export function pushExports(hub: Hub, kind: SlotKind): void {
	if (!streamDeck.ui.action) return;
	void streamDeck.ui.sendToPropertyInspector({ event: "getExports", items: itemsOf(hub.exports, kind) });
}

/** Limits how often `fn` runs, for alerts on a dial that is turned many times. */
export function throttled(fn: () => void, ms: number): () => void {
	let last = 0;
	return () => {
		const now = Date.now();
		if (now - last >= ms) {
			last = now;
			fn();
		}
	};
}
