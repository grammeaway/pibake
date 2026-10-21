// pibake snapshot gate. Preloaded (`node --require`) into a run that evaluates the snapshot
// bundle's modules as the snapshot build does, without starting Pi. It records what module code
// reads or creates at load that a snapshot would freeze or cannot hold, attributed to the bundled
// module whose top-level code did it, and writes the list as JSON to $PIBAKE_TRACE_OUT on exit.
// Which findings are handled is src/passes/snapshot_safety.rs's call. See src/sea.rs.
"use strict";
const fs = require("node:fs");
const os = require("node:os");
const tty = require("node:tty");
const timers = require("node:timers");
const crypto = require("node:crypto");
const Module = require("node:module");

const out = process.env.PIBAKE_TRACE_OUT;
const root = process.env.PIBAKE_SNAPSHOT_ROOT;
const bundle = fs.realpathSync(process.argv[1]);
// esbuild starts each module's code with a `// <path>` comment.
const markers = [];
fs.readFileSync(bundle, "utf8")
	.split("\n")
	.forEach((text, i) => {
		const m = /^\/\/ (\S*[/:]\S*)$/.exec(text);
		if (m) markers.push([i + 1, m[1]]);
	});
const moduleAt = (line) => {
	let lo = 0;
	let hi = markers.length - 1;
	let found = null;
	while (lo <= hi) {
		const mid = (lo + hi) >> 1;
		if (markers[mid][0] <= line) {
			found = markers[mid][1];
			lo = mid + 1;
		} else hi = mid - 1;
	}
	return found;
};

// The innermost module initialiser on the stack (esbuild names each wrapped module's after its
// path), else the bundle's top level: whose load caused the call.
function site() {
	const [prepare, limit] = [Error.prepareStackTrace, Error.stackTraceLimit];
	Error.prepareStackTrace = (_, calls) => calls;
	Error.stackTraceLimit = Infinity;
	const calls = new Error().stack;
	[Error.prepareStackTrace, Error.stackTraceLimit] = [prepare, limit];
	const frames = calls.filter((c) => c.getFileName() === bundle);
	if (frames.length === 0) return null;
	const init = frames.find((c) => c.getFunctionName() === moduleAt(c.getLineNumber()));
	const frame = init ?? frames[frames.length - 1];
	const module = moduleAt(frame.getLineNumber());
	return module && { module, line: frame.getLineNumber() };
}

const findings = [];
let recording = false;
function record(kind, detail) {
	if (recording) return;
	recording = true;
	try {
		const at = site();
		if (at) findings.push({ kind, detail: String(detail), ...at });
	} finally {
		recording = false;
	}
}

const wrap = (object, name, kind, detail) => {
	const real = object[name];
	object[name] = function (...args) {
		record(kind, detail(args));
		return Reflect.apply(real, this, args);
	};
};

process.env = new Proxy(process.env, {
	get(target, key) {
		if (typeof key === "string") record("env", key);
		return Reflect.get(target, key);
	},
	has(target, key) {
		if (typeof key === "string") record("env", key);
		return Reflect.has(target, key);
	},
	ownKeys(target) {
		record("env", "*");
		return Reflect.ownKeys(target);
	},
});
const argv = process.argv;
Object.defineProperty(process, "argv", {
	get() {
		record("argv", "");
		return argv;
	},
	configurable: true,
});
wrap(process, "cwd", "cwd", () => "");
for (const name of ["homedir", "userInfo", "tmpdir", "hostname"]) wrap(os, name, "os", () => name);
wrap(tty, "isatty", "tty", (a) => `isatty(${a[0]})`);
for (const stream of ["stdin", "stdout", "stderr"]) {
	const s = process[stream];
	for (const key of ["isTTY", "columns", "rows"]) {
		const value = s[key];
		Object.defineProperty(s, key, {
			get() {
				record("tty", `${stream}.${key}`);
				return value;
			},
			configurable: true,
		});
	}
}
wrap(Date, "now", "time", () => "Date.now()");
wrap(performance, "now", "time", () => "performance.now()");
const { bigint } = process.hrtime;
wrap(process, "hrtime", "time", () => "process.hrtime()");
process.hrtime.bigint = bigint;
wrap(process.hrtime, "bigint", "time", () => "process.hrtime.bigint()");
globalThis.Date = new Proxy(Date, {
	construct(target, args, newTarget) {
		if (args.length === 0) record("time", "new Date()");
		return Reflect.construct(target, args, newTarget);
	},
});
wrap(Math, "random", "random", () => "Math.random()");
for (const name of ["randomBytes", "randomUUID", "randomInt"]) {
	wrap(crypto, name, "random", () => `crypto.${name}()`);
}
for (const name of ["randomUUID", "getRandomValues"]) wrap(globalThis.crypto, name, "random", () => `crypto.${name}()`);
for (const name of ["setTimeout", "setInterval", "setImmediate"]) {
	wrap(globalThis, name, "timer", () => name);
	wrap(timers, name, "timer", () => name);
}
for (const name of Object.getOwnPropertyNames(Intl)) {
	if (typeof Intl[name] !== "function" || !/^[A-Z]/.test(name)) continue;
	Intl[name] = new Proxy(Intl[name], {
		construct(target, args, newTarget) {
			record("intl", `Intl.${name}`);
			return Reflect.construct(target, args, newTarget);
		},
	});
}
// Builtins whose native state a snapshot cannot hold: snapshot_safety's `LAZY_BUILTINS`.
const unsafe = new Set(process.env.PIBAKE_LAZY_BUILTINS.split(","));
const load = Module._load;
Module._load = function (request, ...rest) {
	const name = request.replace(/^node:/, "");
	if (unsafe.has(name)) record("load", name);
	return Reflect.apply(load, this, [request, ...rest]);
};
for (const name of ["readFileSync", "existsSync", "statSync", "lstatSync", "readdirSync", "realpathSync", "accessSync", "openSync"]) {
	const real = fs[name];
	fs[name] = function (path, ...rest) {
		// Files under the root are the executable's own, content-addressed by its hash.
		if (!(typeof path === "string" && root && path.startsWith(`${root}/`))) record("fs", path);
		return Reflect.apply(real, this, [path, ...rest]);
	};
}
// Extension code is evaluated at launch, never in the snapshot; its live location says so.
let live;
Object.defineProperty(globalThis, "__pibake_live", {
	get: () => live,
	set(fn) {
		live = (rel) => {
			record("live", rel);
			return fn(rel);
		};
	},
	configurable: true,
});

globalThis.__pibake_trace = true;
process.on("exit", () => fs.writeFileSync(out, JSON.stringify(findings)));
