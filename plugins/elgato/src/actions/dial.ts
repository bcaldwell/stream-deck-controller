import {
	action,
	type DialAction,
	type DialDownEvent,
	type DialRotateEvent,
	type DialUpEvent,
	type TouchTapEvent,
} from "@elgato/streamdeck";

import type { Hub } from "../hub";
import { pngDataUrl } from "../tiles";
import { ExportAction, type ExportSettings } from "./shared";

/** Offline, a turned dial would otherwise flash an alert for every tick. */
const ALERT_EVERY_MS = 1_500;

/** A Stream Deck + dial that shows and drives a server-defined dial (`sdc.export` of an `sdc.dial`). */
@action({ UUID: "ca.bcaldwell.sdc.dial" })
export class SdcDial extends ExportAction<DialAction<ExportSettings>> {
	readonly #alertedAt = new Map<string, number>();

	constructor(hub: Hub) {
		super(hub, "dial");
	}

	override onDialRotate(ev: DialRotateEvent<ExportSettings>): void {
		if (!this.hub.rotate(ev.action.id, ev.payload.ticks)) this.#alert(ev.action);
	}

	override onDialDown(ev: DialDownEvent<ExportSettings>): void {
		if (!this.hub.send({ type: "dial_down", slot: ev.action.id })) this.#alert(ev.action);
	}

	override onDialUp(ev: DialUpEvent<ExportSettings>): void {
		this.hub.send({ type: "dial_up", slot: ev.action.id });
	}

	override onTouchTap(ev: TouchTapEvent<ExportSettings>): void {
		if (!this.hub.send({ type: "touch_tap", slot: ev.action.id })) this.#alert(ev.action);
	}

	#alert(dial: DialAction<ExportSettings>): void {
		const now = Date.now();
		if (now - (this.#alertedAt.get(dial.id) ?? 0) < ALERT_EVERY_MS) return;
		this.#alertedAt.set(dial.id, now);
		void dial.showAlert();
	}

	protected show(dial: DialAction<ExportSettings>, exportId: string | undefined): void {
		if (!exportId) {
			void dial.setFeedback({ title: "SDC", value: "choose an export", indicator: 0 });
			return;
		}
		this.hub.subscribe(dial.id, exportId, "dial", {
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
