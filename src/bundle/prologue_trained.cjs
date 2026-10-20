// pibake trained prologue, the banner of the CommonJS bundle the `sea_trained` backend builds,
// inside the function the whole bundle is wrapped in (see src/bundle.rs): `prologue.mjs` as a
// script. Run by `loader_trained.cjs` in an executable, `train.cjs` at build time, or `pi.cjs`.
var __pibake_fs = require("node:fs");
var { homedir: __pibake_homedir } = require("node:os");
var { dirname: __pibake_dirname, isAbsolute: __pibake_isAbsolute, join: __pibake_join } = require("node:path");
var { pathToFileURL: __pibake_pathToFileURL } = require("node:url");
var __pibake_sea = require("node:sea");

function __pibake_start() {
	if (!__pibake_sea.isSea()) return "__PIBAKE_STAGE__";
	const info = JSON.parse(__pibake_sea.getAsset("pibake.json", "utf8"));
	const args = process.argv.slice(2);
	if (args.length === 1 && (args[0] === "--version" || args[0] === "-v")) {
		__pibake_fs.writeSync(1, `${info.version}\n`);
		process.exit(0);
	}
	// What the executable was built from, for the entry's staleness check.
	globalThis.__pibake_setup = info.setup;
	const cache = process.env.XDG_CACHE_HOME;
	const base = __pibake_join(cache && __pibake_isAbsolute(cache) ? cache : __pibake_join(__pibake_homedir(), ".cache"), "pibake");
	const root = __pibake_join(base, info.hash);
	try {
		__pibake_extract(base, root, info.hash);
	} catch (e) {
		throw new Error(`pibake: extracting Pi's files into ${root}: ${e.message}`, { cause: e });
	}
	return root;
}

// pibake:extract

var __pibake_root = __pibake_start();
globalThis.__pibake_at = (rel) => {
	const filename = __pibake_join(__pibake_root, rel);
	return { url: __pibake_pathToFileURL(filename).href, dirname: __pibake_dirname(filename), filename };
};
// Pi's release bundle defines `require` the same way, for CommonJS dependencies.
var require = require("node:module").createRequire(__pibake_join(__pibake_root, "main.cjs"));
