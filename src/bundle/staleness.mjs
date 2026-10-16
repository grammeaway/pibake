// pibake staleness check, imported by the bundle's entry as `pibake:staleness`: compares the
// setup an executable was built from (recorded by src/staleness.rs) with the live one, and names
// what changed. Reads files only; never writes.
import { createHash } from "node:crypto";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { basename, join, resolve } from "node:path";

/** Prints one line to stderr when the setup differs from `recorded`; never throws. */
export function checkSetup(agentDir, recorded) {
	if (!recorded || process.env.PIBAKE_NO_STALENESS_CHECK === "1") return;
	const name = basename(process.execPath);
	let changes;
	try {
		changes = setupChanges(recorded, currentSetup(agentDir, recorded));
	} catch (e) {
		process.stderr.write(`${name}: checking the setup failed: ${e.message}\n`);
		return;
	}
	if (changes.length === 0) return;
	const shown = changes.length > 3 ? [...changes.slice(0, 3), `${changes.length - 3} more`] : changes;
	process.stderr.write(`${name}: built from an older setup (${shown.join(", ")}); run pibake install\n`);
}

/**
 * The setup now, in the recorded shape. Package paths come from `recorded` (they follow from the
 * `packages` entries, compared on their own). An extension file whose size and mtime are as
 * recorded keeps its recorded hash; any other is read and hashed.
 */
export function currentSetup(agentDir, recorded) {
	const settings = readJson(join(agentDir, "settings.json"));
	const known = new Map(recorded.extensions.map((e) => [e.path, e]));
	return {
		pi: { path: recorded.pi.path, version: readJson(recorded.pi.path)?.version ?? null },
		packages: settings?.packages ?? null,
		npm: recorded.npm.map(({ source, path }) => ({
			source,
			path,
			version: readJson(join(resolve(agentDir, path), "package.json"))?.version ?? null,
		})),
		git: recorded.git.map(({ source, path }) => ({ source, path, commit: gitHead(resolve(agentDir, path)) })),
		extensions: extensionFiles(join(agentDir, "extensions")).map(({ path, file, stat }) => {
			if (!stat?.isFile()) return { path, size: null, mtime_ns: null, sha256: null };
			const size = Number(stat.size);
			const mtime_ns = String(stat.mtimeNs);
			const was = known.get(path);
			const sha256 =
				was && was.size === size && was.mtime_ns === mtime_ns
					? was.sha256
					: createHash("sha256").update(readFileSync(file)).digest("hex");
			return { path, size, mtime_ns, sha256 };
		}),
	};
}

/** What differs between two setups, one short phrase each; empty when nothing does. */
export function setupChanges(was, now) {
	const out = [];
	if (was.pi.version !== now.pi.version) out.push(`pi ${was.pi.version} -> ${now.pi.version ?? "missing"}`);
	const removed = new Set();
	if (canonical(was.packages) !== canonical(now.packages)) {
		const before = sources(was.packages);
		const after = sources(now.packages);
		const added = [...after].filter((s) => !before.has(s));
		for (const s of before) if (!after.has(s)) removed.add(s);
		for (const s of added) out.push(`package added: ${s}`);
		for (const s of removed) out.push(`package removed: ${s}`);
		if (added.length === 0 && removed.size === 0) out.push("settings.json packages changed");
	}
	const pairs = (key) => was[key].map((w, i) => [w, now[key][i]]).filter(([w]) => !removed.has(w.source));
	for (const [w, n] of pairs("npm")) {
		if (w.version !== n.version) out.push(`${w.source} ${w.version} -> ${n.version ?? "missing"}`);
	}
	for (const [w, n] of pairs("git")) {
		if (w.commit !== n.commit) out.push(`${w.source} ${short(w.commit)} -> ${short(n.commit) ?? "missing"}`);
	}
	const after = new Map(now.extensions.map((e) => [e.path, e]));
	for (const w of was.extensions) {
		const n = after.get(w.path);
		if (!n) out.push(`extensions/${w.path} removed`);
		else if (n.sha256 !== w.sha256) out.push(`extensions/${w.path} edited`);
		after.delete(w.path);
	}
	for (const path of after.keys()) out.push(`extensions/${path} added`);
	return out;
}

function readJson(file) {
	let text;
	try {
		text = readFileSync(file, "utf8");
	} catch (e) {
		if (e.code === "ENOENT" || e.code === "ENOTDIR") return undefined;
		throw e;
	}
	return JSON.parse(text.replace(/^\uFEFF/, ""));
}

const sources = (packages) =>
	new Set((Array.isArray(packages) ? packages : []).map((p) => (typeof p === "string" ? p : p?.source)).filter((s) => typeof s === "string"));

const short = (commit) => commit?.slice(0, 12) ?? null;

/** JSON with object keys sorted, so key order in settings.json does not count as a change. */
function canonical(value) {
	return JSON.stringify(value, (_key, v) =>
		v && typeof v === "object" && !Array.isArray(v)
			? Object.fromEntries(Object.entries(v).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)))
			: v,
	);
}

/**
 * Files under `dir` as the stage reads them: file symlinks followed, `/`-separated paths, sorted.
 * What `pibake build` would refuse (a symlinked directory, a dangling link) comes with a `null`
 * or non-file stat, so it shows up as a change.
 */
function extensionFiles(dir) {
	const out = [];
	const walk = (sub, prefix) => {
		for (const entry of readdirSync(sub, { withFileTypes: true })) {
			const file = join(sub, entry.name);
			const path = prefix + entry.name;
			let stat = null;
			try {
				stat = statSync(file, { bigint: true });
			} catch {}
			if (stat?.isDirectory() && !entry.isSymbolicLink()) walk(file, `${path}/`);
			else out.push({ path, file, stat });
		}
	};
	try {
		statSync(dir);
	} catch (e) {
		if (e.code === "ENOENT") return out;
		throw e;
	}
	walk(dir, "");
	return out.sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0));
}

/** The commit `HEAD` names, read from the files git keeps it in; `null` when unreadable. */
// shortcut: a `.git` file (worktree, submodule) is not followed; Pi clones into a plain `.git` directory.
function gitHead(dir) {
	const gitDir = join(dir, ".git");
	let ref = "HEAD";
	for (let depth = 0; depth < 5; depth++) {
		let text;
		try {
			text = readFileSync(join(gitDir, ref), "utf8").trim();
		} catch {
			return packedRef(gitDir, ref);
		}
		if (!text.startsWith("ref: ")) return text;
		ref = text.slice("ref: ".length);
	}
	return null;
}

function packedRef(gitDir, ref) {
	let text;
	try {
		text = readFileSync(join(gitDir, "packed-refs"), "utf8");
	} catch {
		return null;
	}
	for (const line of text.split("\n")) {
		if (line.startsWith("#") || line.startsWith("^")) continue;
		const [hash, name] = line.trim().split(" ");
		if (name === ref) return hash;
	}
	return null;
}
