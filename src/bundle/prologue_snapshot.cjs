// pibake snapshot prologue, the banner of the CommonJS bundle the `sea_snapshot` backend builds:
// runs before any bundled module. The snapshot is taken once every module has run at build time,
// so whatever Node cannot serialize, or a launch must read for itself, waits for
// `__pibake_resume`, which the deserialized main calls first: signal listeners, the initialisers
// src/passes/snapshot_safety.rs defers (`__pibake_reinit`), the working directory. Run as plain
// `node main.cjs`, nothing waits. See src/sea.rs.
var __pibake_fs = require("node:fs");
var { dirname: __pibake_dirname, isAbsolute: __pibake_isAbsolute, join: __pibake_join } = require("node:path");
var { pathToFileURL: __pibake_pathToFileURL } = require("node:url");
var __pibake_sea = require("node:sea");
// Building the snapshot, or tracing that build (bundle/trace.cjs).
var __pibake_building =
	require("node:v8").startupSnapshot.isBuildingSnapshot() || globalThis.__pibake_trace !== undefined;
// Module code reads its location while the snapshot is built, so the executable's root is the
// absolute path its files were extracted to at build time.
var __pibake_root = __pibake_building ? __pibake_snapshot_root() : "__PIBAKE_STAGE__";

function __pibake_snapshot_root() {
	const root = process.env.PIBAKE_SNAPSHOT_ROOT;
	if (!root || !__pibake_isAbsolute(root) || !__pibake_fs.existsSync(__pibake_join(root, ".pibake-extracted"))) {
		throw new Error(`pibake: PIBAKE_SNAPSHOT_ROOT must name extracted files, not ${JSON.stringify(root)}`);
	}
	return root;
}

// pibake:extract

globalThis.__pibake_at = (rel) => {
	const filename = __pibake_join(__pibake_root, rel);
	return { url: __pibake_pathToFileURL(filename).href, dirname: __pibake_dirname(filename), filename };
};
// Pi's release bundle defines `require` the same way, for CommonJS dependencies.
var require = require("node:module").createRequire(__pibake_join(__pibake_root, "main.cjs"));

var __pibake_reinits = [];
var __pibake_signals = [];
var __pibake_real_cwd = process.cwd;
var __pibake_listen = ["on", "addListener", "once", "prependListener", "prependOnceListener"].map((name) => [
	name,
	process[name],
]);
if (__pibake_building) {
	for (const [name, real] of __pibake_listen) {
		process[name] = function (event, ...rest) {
			// A signal listener is a native handle, which a snapshot cannot hold.
			if (typeof event === "string" && event.startsWith("SIG")) {
				__pibake_signals.push([real, event, rest]);
				return this;
			}
			return real.call(this, event, ...rest);
		};
	}
}
globalThis.__pibake_reinit = (init) => (__pibake_building ? __pibake_reinits.push(init) : init());

globalThis.__pibake_resume = () => {
	if (!__pibake_building) return;
	if (__pibake_sea.isSea()) {
		const info = JSON.parse(__pibake_sea.getAsset("pibake.json", "utf8"));
		const args = process.argv.slice(2);
		if (args.length === 1 && (args[0] === "--version" || args[0] === "-v")) {
			__pibake_fs.writeSync(1, `${info.version}\n`);
			process.exit(0);
		}
		// What the executable was built from, for the entry's staleness check.
		globalThis.__pibake_setup = info.setup;
		try {
			__pibake_extract(__pibake_dirname(__pibake_root), __pibake_root, info.hash);
		} catch (e) {
			throw new Error(`pibake: extracting Pi's files into ${__pibake_root}: ${e.message}`, { cause: e });
		}
	}
	for (const [name, real] of __pibake_listen) process[name] = real;
	for (const [real, event, rest] of __pibake_signals.splice(0)) real.call(process, event, ...rest);
	// Clears working-directory memos that reset on chdir: Node's own, and graceful-fs's, which
	// holds the build's.
	try {
		process.chdir(".");
	} catch {}
	if (process.cwd() !== __pibake_real_cwd.call(process)) {
		throw new Error(`pibake: process.cwd() still says the build's ${process.cwd()}`);
	}
	for (const init of __pibake_reinits.splice(0)) init();
};
