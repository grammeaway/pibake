// pibake bundle prologue, the esbuild banner: runs before any bundled module. Sets the root that
// bundled code locates itself under (`__pibake_at`) and resolves `require` from: the stage, or
// in a pibake executable, the stage files it carries, extracted once into the user's cache.
// See src/bundle.rs and src/sea.rs.
import { createRequire as __pibake_createRequire } from "node:module";
import * as __pibake_fs from "node:fs";
import { homedir as __pibake_homedir } from "node:os";
import { dirname as __pibake_dirname, isAbsolute as __pibake_isAbsolute, join as __pibake_join } from "node:path";
import { pathToFileURL as __pibake_pathToFileURL } from "node:url";
import * as __pibake_sea from "node:sea";

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

// Each launch extracts into its own temporary directory and renames it into place; a launch that
// loses the race uses the winner's. A directory without the marker was not made here.
function __pibake_extract(base, root, hash) {
	const marker = ".pibake-extracted";
	if (__pibake_fs.existsSync(__pibake_join(root, marker))) return;
	__pibake_fs.mkdirSync(base, { recursive: true });
	const tmp = __pibake_fs.mkdtempSync(__pibake_join(base, `.tmp-${hash}-`));
	try {
		for (const key of __pibake_sea.getAssetKeys()) {
			if (!key.startsWith("files/")) continue;
			const file = __pibake_join(tmp, key.slice("files/".length));
			__pibake_fs.mkdirSync(__pibake_dirname(file), { recursive: true });
			__pibake_fs.writeFileSync(file, new Uint8Array(__pibake_sea.getRawAsset(key)));
		}
		__pibake_fs.writeFileSync(__pibake_join(tmp, marker), `${hash}\n`);
		try {
			__pibake_fs.renameSync(tmp, root);
		} catch (e) {
			if (!__pibake_fs.existsSync(__pibake_join(root, marker))) {
				throw new Error(`${root} exists without ${marker}; remove it`, { cause: e });
			}
		}
	} finally {
		__pibake_fs.rmSync(tmp, { recursive: true, force: true });
	}
}

const __pibake_root = __pibake_start();
globalThis.__pibake_at = (rel) => {
	const filename = __pibake_join(__pibake_root, rel);
	return { url: __pibake_pathToFileURL(filename).href, dirname: __pibake_dirname(filename), filename };
};
// Pi's release bundle defines `require` the same way, for CommonJS dependencies.
const require = __pibake_createRequire(__pibake_join(__pibake_root, "main.mjs"));
