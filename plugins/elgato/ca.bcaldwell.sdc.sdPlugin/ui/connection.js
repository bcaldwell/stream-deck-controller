// The plugin-wide "Connection" section (Server URL, Token, Deck layout), shared by every
// property inspector: each page only has an empty <div id="connection"></div> and loads this
// script after sdpi-components. The settings are global; the plugin pushes the live status.

const section = document.getElementById("connection");
section.innerHTML = `
	<details>
		<summary id="connection-status">Connection</summary>
		<sdpi-item label="Server URL">
			<sdpi-textfield global setting="url" placeholder="your-server.example.com"></sdpi-textfield>
		</sdpi-item>
		<sdpi-item label="Token">
			<sdpi-password global setting="token"></sdpi-password>
		</sdpi-item>
		<sdpi-item label="Deck layout">
			<sdpi-textfield global setting="layout" placeholder="empty = server default"></sdpi-textfield>
		</sdpi-item>
		<sdpi-item>
			<small>Deck layout is only used by SDC Deck Key: the sdc.device name to show.</small>
		</sdpi-item>
	</details>`;

const details = section.querySelector("details");
const line = section.querySelector("#connection-status");
let wasConnected;

SDPIComponents.streamDeckClient.sendToPropertyInspector.subscribe(({ payload }) => {
	if (payload.event !== "status") return;
	const { host, connected, reason } = payload;
	if (connected) {
		line.textContent = `● Connected to ${host}`;
		line.style.color = "#3fb950";
	} else {
		line.textContent = host ? `Can't connect: ${reason || "waiting for the server"}` : "Not configured";
		line.style.color = host ? "#f85149" : "";
	}
	// collapse once connected, open whenever it is not; leave it alone while status repeats
	if (connected !== wasConnected) details.open = !connected;
	wasConnected = connected;
});
