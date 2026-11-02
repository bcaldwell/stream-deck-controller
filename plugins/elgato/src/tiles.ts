// Small SVG tiles for states the server cannot draw (not configured, export error).

const svg = (body: string, bg: string): string =>
	"data:image/svg+xml;charset=utf8," +
	encodeURIComponent(
		`<svg xmlns="http://www.w3.org/2000/svg" width="144" height="144" viewBox="0 0 144 144">` +
			`<rect width="144" height="144" fill="${bg}"/>${body}</svg>`,
	);

const escape = (s: string): string => s.replace(/[<>&"]/g, (c) => `&#${c.charCodeAt(0)};`);

const lines = (text: string[], y0: number, size: number): string =>
	text
		.map(
			(t, i) =>
				`<text x="72" y="${y0 + i * (size + 6)}" font-family="Helvetica, Arial, sans-serif" font-size="${size}" ` +
				`font-weight="600" text-anchor="middle" fill="#ffffff">${escape(t)}</text>`,
		)
		.join("");

/** Shown on a key that has no export chosen yet. */
export const unconfiguredTile = (): string => svg(lines(["SDC", "choose an", "export"], 52, 20), "#1c1c1e");

/** Shown on a key whose export the server cannot render. */
export const errorTile = (message: string): string => {
	const short = message.length > 36 ? `${message.slice(0, 35)}…` : message;
	const words = short.split(/\s+/);
	const rows: string[] = [];
	for (const w of words) {
		const last = rows[rows.length - 1];
		if (last !== undefined && last.length + w.length < 13) rows[rows.length - 1] = `${last} ${w}`;
		else rows.push(w);
	}
	return svg(lines(["ERR", ...rows.slice(0, 3)], 44, 18), "#b00020");
};

export const pngDataUrl = (b64: string): string => `data:image/png;base64,${b64}`;
