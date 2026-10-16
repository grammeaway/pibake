// pibake frozen-settings check, imported by the bundle's entry as `pibake:frozen`. A pass that
// bakes a setting into the build records it (src/passes.rs `Frozen`); this names whatever the
// live setup or the command line asks for that the build left out. Reads files only; never
// writes, never exits: Pi starts either way, and a request to a left-out API fails on its own.
import { readFileSync } from "node:fs";
import { basename, join } from "node:path";

/** Prints one line to stderr per frozen setting the setup asks past; never throws. */
export function checkFrozen(agentDir, argv, frozen) {
	if (frozen.length === 0) return;
	const name = basename(process.execPath);
	for (const f of frozen) {
		let asked;
		try {
			asked = leftOut(f, agentDir, argv);
		} catch (e) {
			process.stderr.write(`${name}: checking ${f.setting} failed: ${e.message}\n`);
			continue;
		}
		if (asked.length === 0) continue;
		const shown = asked.length > 3 ? [...asked.slice(0, 3), `${asked.length - 3} more`] : asked;
		process.stderr.write(
			`${name}: this build left out ${shown.join(", ")}; add ${asked.length > 1 ? "them" : "it"} to ${f.setting} in pibake.toml, then run pibake install\n`,
		);
	}
}

/** What the setup asks for that `f` left out, as `<item> (<who asks>)`, in the order asked. */
export function leftOut(f, agentDir, argv) {
	switch (f.kind) {
		case "apis":
			return leftOutApis(f, agentDir, argv);
		default:
			throw new Error(`pibake has no check for ${f.kind}`);
	}
}

// shortcut: project settings (`.pi/settings.json`) and provider-less model patterns are not
// checked; a request to a left-out API still fails with its own message.
function leftOutApis(f, agentDir, argv) {
	const settings = readJson(join(agentDir, "settings.json")) ?? {};
	const custom = readJson(join(agentDir, "models.json"))?.providers ?? {};
	const known = (p) => Object.hasOwn(f.providers, p) || Object.hasOwn(custom, p);
	const apisOf = (p) => {
		const apis = new Set(f.providers[p] ?? []);
		const own = Object.hasOwn(custom, p) ? custom[p] : undefined;
		for (const entry of [own, ...(Array.isArray(own?.models) ? own.models : [])]) {
			if (typeof entry?.api === "string") apis.add(entry.api);
		}
		return apis;
	};
	const asked = [];
	const ask = (p, who) => {
		for (const api of apisOf(p)) {
			const item = `the ${api} API (${who})`;
			if (f.left_out.includes(api) && !asked.includes(item)) asked.push(item);
		}
	};
	const askPattern = (pattern, who) => {
		const slash = pattern.indexOf("/");
		if (slash > 0 && known(pattern.slice(0, slash))) ask(pattern.slice(0, slash), who);
	};

	for (let i = 0; i + 1 < argv.length; i++) {
		const value = argv[i + 1];
		if (argv[i] === "--provider") ask(value, `--provider ${value}`);
		else if (argv[i] === "--model") askPattern(value, `--model ${value}`);
		else if (argv[i] === "--models") for (const p of value.split(",")) askPattern(p.trim(), `--models ${p.trim()}`);
	}
	if (typeof settings.defaultProvider === "string") {
		ask(settings.defaultProvider, `settings.json defaultProvider ${settings.defaultProvider}`);
	}
	for (const p of Array.isArray(settings.enabledModels) ? settings.enabledModels : []) {
		if (typeof p === "string") askPattern(p, `settings.json enabledModels ${p}`);
	}
	for (const p of Object.keys(custom)) ask(p, `models.json provider ${p}`);
	return asked;
}

/** `null` when missing or not JSON: Pi reports those itself. */
function readJson(path) {
	try {
		return JSON.parse(readFileSync(path, "utf8"));
	} catch {
		return null;
	}
}
