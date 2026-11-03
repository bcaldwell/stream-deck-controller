import {
	streamDeck,
	SingletonAction,
	type DialAction,
	type DidReceiveSettingsEvent,
	type KeyAction,
	type WillAppearEvent,
	type WillDisappearEvent,
} from "@elgato/streamdeck";

import type { Hub } from "../hub";
import type { SlotKind } from "../protocol";

/** Per-action settings, written by the property inspector. */
export type ExportSettings = { export?: string };

/** How long the property inspector's dropdown waits for the server's list. */
const EXPORTS_WAIT_MS = 4_000;

/** Sends the exports of `kind` to the open property inspector's dropdown. */
export async function sendExports(hub: Hub, kind: SlotKind): Promise<void> {
	const items = hub.exports
		.filter((e) => e.kind === kind)
		.map((e) => ({ label: e.label ? `${e.label} (${e.id})` : e.id, value: e.id }));
	await streamDeck.ui.sendToPropertyInspector({ event: "getExports", items });
}

/** Subscribes the action instance to its chosen export and unsubscribes when it goes away. */
export abstract class ExportAction<
	A extends KeyAction<ExportSettings> | DialAction<ExportSettings>,
> extends SingletonAction<ExportSettings> {
	protected readonly hub: Hub;
	readonly #kind: SlotKind;

	constructor(hub: Hub, kind: SlotKind) {
		super();
		this.hub = hub;
		this.#kind = kind;
	}

	/** Draws the placeholder if `exportId` is undefined, otherwise `hub.subscribe`s. */
	protected abstract show(action: A, exportId: string | undefined): void;

	override onWillAppear(ev: WillAppearEvent<ExportSettings>): void {
		this.#attach(ev.action, ev.payload.settings);
	}

	override onDidReceiveSettings(ev: DidReceiveSettingsEvent<ExportSettings>): void {
		this.#attach(ev.action, ev.payload.settings);
	}

	override onWillDisappear(ev: WillDisappearEvent<ExportSettings>): void {
		this.hub.unsubscribe(ev.action.id);
	}

	/** The property inspector's dropdown (`datasource="getExports"`) asks for the exports of our kind. */
	override async onSendToPlugin(): Promise<void> {
		await this.hub.waitForExports(EXPORTS_WAIT_MS);
		await sendExports(this.hub, this.#kind);
	}

	#attach(action: WillAppearEvent<ExportSettings>["action"], settings: ExportSettings): void {
		this.hub.unsubscribe(action.id);
		// The manifest limits each action to one controller type, so `action` is an `A`.
		this.show(action as unknown as A, settings.export || undefined);
	}
}
