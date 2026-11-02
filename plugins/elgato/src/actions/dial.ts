import {
	action,
	SingletonAction,
	type DialAction,
	type DialDownEvent,
	type DialRotateEvent,
	type DialUpEvent,
	type DidReceiveSettingsEvent,
	type SendToPluginEvent,
	type TouchTapEvent,
	type WillAppearEvent,
	type WillDisappearEvent,
} from "@elgato/streamdeck";
import type { JsonObject, JsonValue } from "@elgato/utils";

import type { Hub } from "../hub";
import { pngDataUrl } from "../tiles";
import { answerExports, exportId, throttled, type ExportSettings } from "./shared";

/** A Stream Deck + dial that shows and drives a server-defined dial (`sdc.export` of an `sdc.dial`). */
@action({ UUID: "ca.bcaldwell.sdc.dial" })
export class SdcDial extends SingletonAction<ExportSettings> {
	readonly #hub: Hub;
	readonly #alerts = new Map<string, () => void>();

	constructor(hub: Hub) {
		super();
		this.#hub = hub;
	}

	override onWillAppear(ev: WillAppearEvent<ExportSettings>): void {
		if (ev.action.isDial()) this.#attach(ev.action, ev.payload.settings);
	}

	override onWillDisappear(ev: WillDisappearEvent<ExportSettings>): void {
		this.#hub.unsubscribe(ev.action.id);
		this.#alerts.delete(ev.action.id);
	}

	override onDidReceiveSettings(ev: DidReceiveSettingsEvent<ExportSettings>): void {
		if (ev.action.isDial()) this.#attach(ev.action, ev.payload.settings);
	}

	override onDialRotate(ev: DialRotateEvent<ExportSettings>): void {
		if (!this.#hub.rotate(ev.action.id, ev.payload.ticks)) this.#alert(ev.action);
	}

	override onDialDown(ev: DialDownEvent<ExportSettings>): void {
		if (!this.#hub.send({ type: "dial_down", slot: ev.action.id })) this.#alert(ev.action);
	}

	override onDialUp(ev: DialUpEvent<ExportSettings>): void {
		this.#hub.send({ type: "dial_up", slot: ev.action.id });
	}

	override onTouchTap(ev: TouchTapEvent<ExportSettings>): void {
		if (!this.#hub.send({ type: "touch_tap", slot: ev.action.id })) this.#alert(ev.action);
	}

	override async onSendToPlugin(ev: SendToPluginEvent<JsonValue, JsonObject>): Promise<void> {
		await answerExports(this.#hub, ev, "dial");
	}

	/** Offline: a turned dial would otherwise flash an alert for every tick. */
	#alert(dial: DialAction<ExportSettings>): void {
		let alert = this.#alerts.get(dial.id);
		if (!alert) {
			alert = throttled(() => void dial.showAlert(), 1_500);
			this.#alerts.set(dial.id, alert);
		}
		alert();
	}

	#attach(dial: DialAction<ExportSettings>, settings: ExportSettings): void {
		const id = exportId(settings);
		this.#hub.unsubscribe(dial.id);
		if (!id) {
			void dial.setFeedback({ title: "SDC", value: "choose an export", indicator: 0 });
			return;
		}
		this.#hub.subscribe(dial.id, id, "dial", {
			dialFeedback: (f) =>
				void dial.setFeedback({
					title: f.title,
					value: f.value,
					indicator: f.indicator,
					// an absent icon is cleared, so a dial that stops sending one does not keep a stale picture
					icon: f.icon_png_base64 ? pngDataUrl(f.icon_png_base64) : "",
				}),
			error: (message) => {
				void dial.setFeedback({ title: "ERR", value: message.slice(0, 30), indicator: 0 });
				void dial.showAlert();
			},
		});
	}
}
