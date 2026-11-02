import {
	action,
	SingletonAction,
	type DidReceiveSettingsEvent,
	type KeyAction,
	type KeyDownEvent,
	type KeyUpEvent,
	type SendToPluginEvent,
	type WillAppearEvent,
	type WillDisappearEvent,
} from "@elgato/streamdeck";
import type { JsonObject, JsonValue } from "@elgato/utils";

import type { Hub } from "../hub";
import { errorTile, pngDataUrl, unconfiguredTile } from "../tiles";
import { answerExports, exportId, type ExportSettings } from "./shared";

/** A key that shows and presses a server-defined button (`sdc.export` of an `sdc.button`). */
@action({ UUID: "ca.bcaldwell.sdc.button" })
export class SdcButton extends SingletonAction<ExportSettings> {
	readonly #hub: Hub;

	constructor(hub: Hub) {
		super();
		this.#hub = hub;
	}

	override onWillAppear(ev: WillAppearEvent<ExportSettings>): void {
		if (ev.action.isKey()) this.#attach(ev.action, ev.payload.settings);
	}

	override onWillDisappear(ev: WillDisappearEvent<ExportSettings>): void {
		this.#hub.unsubscribe(ev.action.id);
	}

	override onDidReceiveSettings(ev: DidReceiveSettingsEvent<ExportSettings>): void {
		if (ev.action.isKey()) this.#attach(ev.action, ev.payload.settings);
	}

	override async onKeyDown(ev: KeyDownEvent<ExportSettings>): Promise<void> {
		if (!this.#hub.send({ type: "slot_down", slot: ev.action.id })) await ev.action.showAlert();
	}

	override async onKeyUp(ev: KeyUpEvent<ExportSettings>): Promise<void> {
		this.#hub.send({ type: "slot_up", slot: ev.action.id });
	}

	override async onSendToPlugin(ev: SendToPluginEvent<JsonValue, JsonObject>): Promise<void> {
		await answerExports(this.#hub, ev, "key");
	}

	#attach(key: KeyAction<ExportSettings>, settings: ExportSettings): void {
		const id = exportId(settings);
		this.#hub.unsubscribe(key.id);
		if (!id) {
			void key.setTitle("");
			void key.setImage(unconfiguredTile());
			return;
		}
		void key.setTitle("");
		this.#hub.subscribe(key.id, id, "key", {
			keyImage: (png) => void key.setImage(pngDataUrl(png)),
			error: (message) => {
				void key.setImage(errorTile(message));
				void key.showAlert();
			},
		});
	}
}

