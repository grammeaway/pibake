// pibake bundle driver: runs the staged esbuild with pibake's plugins, prints a JSON result.
// Usage: node build.mjs <config.json>. Generated per build by `pibake bundle`; see src/bundle.rs.
import { readFile } from "node:fs/promises";
import { dirname, extname, isAbsolute, join, relative, sep } from "node:path";
import { pathToFileURL } from "node:url";

const config = JSON.parse(await readFile(process.argv[2], "utf8"));
const esbuild = await import(pathToFileURL(config.esbuild).href);
const piPackages = join(config.stage, "node_modules", "@earendil-works") + sep;
const loaderDir = join(config.pi_dir, "dist", "core", "extensions");
// Pi's own code sees the location of Pi's release chunks, as in stock Pi: its workers and
// createRequire lookups resolve there.
const piChunk = relative(config.stage, join(config.pi_dir, "dist", "bundle", "chunks", "pibake.js"));

const isPi = (file) => file.startsWith(piPackages);
const under = (file, root) => file.startsWith(root + sep);

/** The module ids Pi hands to extensions (`VIRTUAL_MODULES`), each with the specifier Pi imports it by. */
async function hostModules() {
	const file = join(loaderDir, "virtual-modules.js");
	const text = await readFile(file, "utf8");
	const imports = new Map([...text.matchAll(/^import \* as (\w+) from "([^"]+)";$/gm)].map((m) => [m[1], m[2]]));
	const body = /export const VIRTUAL_MODULES = \{([\s\S]*?)\n\};/.exec(text)?.[1];
	if (!body || imports.size === 0) throw new Error(`pibake: VIRTUAL_MODULES not found in ${file}`);
	const modules = new Map();
	for (const m of body.matchAll(/^\s*(?:"([^"]+)"|(\w+)): (\w+),$/gm)) {
		const specifier = imports.get(m[3]);
		if (!specifier) throw new Error(`pibake: ${file}: ${m[3]} is not imported`);
		modules.set(m[1] ?? m[2], specifier);
	}
	if (modules.size === 0) throw new Error(`pibake: no entries in VIRTUAL_MODULES of ${file}`);
	return { dir: dirname(file), modules };
}

const located = {};
// Files a pibake pass rewrote (src/passes.rs): read instead of the stage's copy.
const overrides = new Map(Object.entries(config.overrides ?? {}));
const overridden = new Set();
const fixes = [];
const shimmed = new Set();
const loaders = { ".ts": "ts", ".mts": "ts", ".cts": "ts", ".tsx": "tsx", ".jsx": "jsx" };

const plugin = {
	name: "pibake",
	async setup(build) {
		const host = await hostModules();
		const targets = new Map();
		// esbuild allows resolve() only once setup is done.
		const target = (id) => {
			if (!targets.has(id)) {
				const specifier = host.modules.get(id);
				targets.set(
					id,
					build.resolve(specifier, { resolveDir: host.dir, kind: "import-statement" }).then((r) => {
						if (r.errors.length) throw new Error(`pibake: resolving ${specifier} for ${id}: ${r.errors[0].text}`);
						return r.path;
					}),
				);
			}
			return targets.get(id);
		};
		// Extensions get Pi's own instances of the host packages, as Pi's jiti virtualModules give them.
		const hostFilter = new RegExp(`^(${[...host.modules.keys()].map((k) => k.replace(/[/.]/g, "\\$&")).join("|")})$`);
		build.onResolve({ filter: hostFilter }, async (args) => {
			if (args.namespace !== "file" || !args.importer || isPi(args.importer)) return undefined;
			return { path: await target(args.path) };
		});

		// jiti tries file extensions on package.json `imports` targets (`"#src/*": "./src/*"`
		// with `#src/a` meaning `./src/a.ts`); Node and esbuild do not. Only used where esbuild fails.
		build.onResolve({ filter: /^#/ }, async (args) => {
			if (args.pluginData?.pibake_retry || !args.importer || isPi(args.importer)) return undefined;
			const opts = { importer: args.importer, resolveDir: args.resolveDir, kind: args.kind, pluginData: { pibake_retry: true } };
			const first = await build.resolve(args.path, opts);
			if (!first.errors.length) return first;
			const mapped = await packageImport(args.path, args.importer);
			if (!mapped) return undefined;
			const r = await build.resolve(mapped.target, { ...opts, resolveDir: mapped.dir });
			if (r.errors.length) return undefined;
			fixes.push({ file: relative(config.stage, args.importer), fix: `${args.path} -> ${relative(config.stage, r.path)}` });
			return r;
		});

		// Pi's extension loader gets jiti through these two modules; both are replaced with a jiti
		// that serves baked extensions and hands anything else to the real one.
		build.onResolve({ filter: /^jiti(\/static)?$/ }, (args) => {
			if (args.importer === join(loaderDir, "jiti-loader.js") || args.importer === join(loaderDir, "jiti-static-loader.js")) {
				return { path: args.path, namespace: "pibake-jiti" };
			}
			return undefined;
		});
		build.onLoad({ filter: /.*/, namespace: "pibake-jiti" }, (args) => {
			shimmed.add(args.path);
			return { contents: jitiShim(args.path), resolveDir: loaderDir, loader: "js" };
		});
		build.onResolve({ filter: /^pibake:real-jiti:/ }, async (args) => {
			const r = await build.resolve(args.path.slice("pibake:real-jiti:".length), {
				resolveDir: loaderDir,
				kind: "dynamic-import",
			});
			if (r.errors.length) throw new Error(`pibake: ${r.errors[0].text}`);
			return { path: r.path };
		});
		build.onResolve({ filter: /^pibake:staleness$/ }, () => ({ path: "staleness", namespace: "pibake-staleness" }));
		build.onLoad({ filter: /.*/, namespace: "pibake-staleness" }, () => ({ contents: config.staleness, loader: "js" }));
		build.onResolve({ filter: /^pibake:frozen$/ }, () => ({ path: "frozen", namespace: "pibake-frozen" }));
		build.onLoad({ filter: /.*/, namespace: "pibake-frozen" }, () => ({ contents: config.frozen, loader: "js" }));
		build.onResolve({ filter: /^pibake:baked$/ }, () => ({ path: "baked", namespace: "pibake-baked" }));
		build.onLoad({ filter: /.*/, namespace: "pibake-baked" }, () => ({
			contents: config.baked,
			resolveDir: config.stage,
			loader: "js",
		}));

		build.onLoad({ filter: /\.[cm]?[jt]sx?$/ }, async (args) => {
			const override = overrides.get(args.path);
			if (override !== undefined) overridden.add(args.path);
			const text = override ?? (await readFile(args.path, "utf8"));
			const plain = override === undefined ? undefined : { contents: override, loader: loaders[extname(args.path)] ?? "js" };
			if (!/import\.meta|__dirname|__filename/.test(text)) return plain;
			const owned = config.owned.find((o) => under(args.path, o.stage));
			const live = owned && !args.path.slice(owned.stage.length).split(sep).includes("node_modules");
			let kind;
			let location;
			let define = LOCATION_DEFINES;
			// Stage locations are relative to the root the prologue sets, so an executable can
			// carry the files they point at.
			if (isPi(args.path)) {
				kind = "pi";
				location = `globalThis.__pibake_at(${JSON.stringify(piChunk)})`;
				define = { "import.meta.url": LOCATION_DEFINES["import.meta.url"] };
			} else if (live) {
				// Extension files see where they are installed in the agent directory, as under jiti.
				kind = "live";
				const rel = join(owned.live, args.path.slice(owned.stage.length + 1));
				location = `globalThis.__pibake_live(${JSON.stringify(rel)})`;
			} else {
				kind = "stage";
				const rel = relative(config.stage, args.path);
				if (rel.startsWith(`..${sep}`) || isAbsolute(rel)) throw new Error(`pibake: ${args.path} is outside the stage ${config.stage}`);
				location = `globalThis.__pibake_at(${JSON.stringify(rel)})`;
			}
			const out = await esbuild.transform(text, {
				loader: loaders[extname(args.path)] ?? "js",
				define,
				sourcemap: "inline",
				sourcefile: args.path,
				tsconfigRaw: {},
			});
			// Mentioned only in strings or comments: nothing to rewrite.
			if (!out.code.includes("__pibake_location")) return plain;
			located[relative(config.stage, args.path)] = kind;
			// A hashbang must stay first; the bundle drops it from non-entry modules anyway.
			const code = out.code.startsWith("#!") ? out.code.slice(out.code.indexOf("\n") + 1) : out.code;
			return { contents: `const __pibake_location = ${location};${code}`, loader: "js" };
		});
	},
};

/** `specifier` through the `imports` field of the package.json nearest to `importer`, as a relative path. */
async function packageImport(specifier, importer) {
	for (let dir = dirname(importer); dir !== dirname(dir); dir = dirname(dir)) {
		let manifest;
		try {
			manifest = JSON.parse(await readFile(join(dir, "package.json"), "utf8"));
		} catch {
			continue;
		}
		for (const [pattern, value] of Object.entries(manifest.imports ?? {})) {
			const star = pattern.indexOf("*");
			let rest;
			if (star < 0) rest = pattern === specifier ? "" : undefined;
			else if (specifier.startsWith(pattern.slice(0, star)) && specifier.endsWith(pattern.slice(star + 1))) {
				rest = specifier.slice(star, specifier.length - (pattern.length - star - 1));
			}
			const target = rest === undefined ? undefined : conditionTarget(value);
			if (typeof target === "string" && target.startsWith("./")) return { dir, target: target.replace("*", rest) };
		}
		return undefined;
	}
	return undefined;
}

function conditionTarget(value) {
	if (typeof value === "string" || value === null) return value;
	for (const key of ["import", "node", "default"]) if (key in value) return conditionTarget(value[key]);
	return undefined;
}

const LOCATION_DEFINES = {
	"import.meta.url": "__pibake_location.url",
	"import.meta.dirname": "__pibake_location.dirname",
	"import.meta.filename": "__pibake_location.filename",
	__dirname: "__pibake_location.dirname",
	__filename: "__pibake_location.filename",
};

function jitiShim(id) {
	return `import { bakedLoader } from "pibake:baked";
export function createJiti(id, options) {
	let real;
	return {
		async import(path, opts) {
			const load = bakedLoader(path);
			if (load) {
				const mod = await load();
				return opts?.default ? (mod?.default ?? mod) : mod;
			}
			real ??= (await import("pibake:real-jiti:${id}")).createJiti(id, options);
			return real.import(path, opts);
		},
	};
}
`;
}

const result = await esbuild.build({
	stdin: { contents: config.entry, resolveDir: config.pi_dir, sourcefile: "entry.mjs", loader: "js" },
	outfile: config.outfile,
	absWorkingDir: config.stage,
	bundle: true,
	platform: "node",
	format: "esm",
	sourcemap: "linked",
	metafile: true,
	write: config.write ?? true,
	logLevel: "silent",
	logLimit: 0,
	logOverride: { "unsupported-dynamic-import": "warning", "unsupported-require-call": "warning" },
	tsconfigRaw: {},
	// No bundled module is the program's main, as none is under jiti; in an executable main.mjs
	// would otherwise be, and run every `if (import.meta.main)` self-check.
	define: { PI_BUNDLED_NODE: "true", "import.meta.main": "false" },
	banner: { js: config.prologue },
	plugins: [plugin],
}).catch((e) => ({ errors: e.errors ?? [{ text: String(e) }], warnings: e.warnings ?? [], metafile: null }));
// Without the stand-in the bundle would still work, transpiling every extension from disk.
for (const id of ["jiti", "jiti/static"]) {
	if (result.errors.length === 0 && !shimmed.has(id)) {
		result.errors.push({ text: `pibake: Pi's extension loader no longer imports "${id}" from ${loaderDir}/jiti-*loader.js` });
	}
}

// A rewrite esbuild never loaded means Pi stopped importing that file: the pass did nothing.
for (const file of overrides.keys()) {
	if (result.errors.length === 0 && !overridden.has(file)) {
		result.errors.push({ text: `pibake: a pass rewrote ${file}, but the bundle never loads it` });
	}
}

const message = (m) => ({ text: m.text, id: m.id || null, file: m.location?.file ?? null, line: m.location?.line ?? null, line_text: m.location?.lineText ?? null });
process.stdout.write(
	JSON.stringify({
		errors: result.errors.map(message),
		warnings: result.warnings.map(message),
		metafile: result.metafile,
		located,
		fixes,
	}),
);
