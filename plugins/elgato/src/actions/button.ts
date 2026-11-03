import { action, type KeyAction, type KeyDownEvent, type KeyUpEvent } from "@elgato/streamdeck";

import type { Hub } from "../hub";
import { errorTile, pngDataUrl, unconfiguredTile } from "../tiles";
import { ExportAction, type ExportSettings } from "./shared";

/** A key that shows and presses a server-defined button (`sdc.export` of an `sdc.button`). */
@action({ UUID: "ca.bcaldwell.sdc.button" })
export class SdcButton extends ExportAction<KeyAction<ExportSettings>> {
	constructor(hub: Hub) {
		super(hub, "key");
	}

	override async onKeyDown(ev: KeyDownEvent<ExportSettings>): Promise<void> {
		if (!this.hub.send({ type: "slot_down", slot: ev.action.id })) await ev.action.showAlert();
	}

	override onKeyUp(ev: KeyUpEvent<ExportSettings>): void {
		this.hub.send({ type: "slot_up", slot: ev.action.id });
	}

	protected show(key: KeyAction<ExportSettings>, exportId: string | undefined): void {
		void key.setTitle("");
		if (!exportId) {
			void key.setImage(unconfiguredTile());
			return;
		}
		this.hub.subscribe(key.id, exportId, "key", {
			keyImage: (png) => void key.setImage(pngDataUrl(png)),
			error: (message) => {
				void key.setImage(errorTile(message));
				void key.showAlert();
			},
		});
	}
}
