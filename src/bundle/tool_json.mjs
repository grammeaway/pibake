// pibake:tool-json: pi-ai's per-delta `parseStreamingJson(buffer)`, incrementally (pass
// incremental_tool_json, src/passes/incremental_tool_json.rs). Every result equals stock's.
//
// A lexer per tool call reads only each new delta. While the buffer is a strict JSON prefix that
// ends inside a string value, stock's result is its result at the delta that opened the string,
// with that string's partial value swapped in, so only the delta is decoded. Anything else (keys,
// numbers, structure, input stock has to repair, a self-check that fails) calls stock.
// shortcut: once a call needs stock's repair (raw control characters, bad escapes) or misparse
// (`[ ]`), it stays on stock, quadratic, to its end; port repair if a provider sends such text.

const states = new WeakMap();

// Lexer states.
const VALUE = 0; // a value is next
const FIRST_VALUE = 1; // after `[`: a value or `]`
const FIRST_KEY = 2; // after `{`: a key or `}`
const KEY = 3; // after `,` in an object
const COLON = 4;
const AFTER = 5; // after a value: `,` or the container's close
const STRING = 6;
const LITERAL = 7;
const NUMBER = 8;
const DONE = 9; // the root value is complete

const HEX = /^[0-9a-fA-F]$/;
const PLAIN = /[^"\\\u0000-\u001f]*/y;

/**
 * `parse(text)` for the tool call `key`, where `text` is the previous call's text plus `delta`.
 * A `delta` that is not a string, or does not account for the growth, rescans `text`.
 */
export function streamToolJson(parse, key, text, delta) {
	if (typeof key !== "object" || key === null || typeof text !== "string") return parse(text);
	let s = states.get(key);
	if (s?.stock) return parse(text);
	if (s && typeof delta === "string" && s.len + delta.length === text.length) {
		feed(s, delta);
	} else {
		s = fresh();
		states.set(key, s);
		feed(s, text);
	}
	s.len = text.length;
	if (s.stock || s.st !== STRING || s.isKey) {
		s.base = undefined;
		return parse(text);
	}
	commit(s);
	if (s.base !== undefined && s.baseId === s.stringId) return rebuild(s.base, s.path, 0, s.decoded);
	const result = parse(text);
	s.path = s.stack.map((f) => (f.arr ? f.count : f.key));
	if (leaf(result, s.path) !== s.decoded) {
		s.stock = true;
		return result;
	}
	s.base = rebuild(result, s.path, 0, s.decoded);
	s.baseId = s.stringId;
	return result;
}

function fresh() {
	return {
		stock: false,
		len: 0,
		st: VALUE,
		stack: [],
		isKey: false,
		esc: 0, // in a string: 0, or 1 after `\`, or 2 + hex digits read after `\u`
		raw: "", // the current string's text not yet decoded
		decoded: "",
		stringId: 0,
		literal: "",
		num: "",
		base: undefined,
		baseId: -1,
		path: [],
	};
}

function feed(s, chunk) {
	let i = 0;
	const n = chunk.length;
	while (i < n && !s.stock) {
		if (s.st === STRING) {
			i = string(s, chunk, i);
			continue;
		}
		const c = chunk[i];
		if (s.st === LITERAL) {
			if (c !== s.literal[0]) return void (s.stock = true);
			s.literal = s.literal.slice(1);
			i++;
			if (s.literal === "") valueDone(s);
			continue;
		}
		if (s.st === NUMBER) {
			if ("0123456789+-.eE".includes(c)) {
				s.num += c;
				i++;
				continue;
			}
			if (!/^-?(0|[1-9]\d*)(\.\d+)?([eE][+-]?\d+)?$/.test(s.num)) return void (s.stock = true);
			valueDone(s);
			continue;
		}
		i++;
		if (c === " " || c === "\t" || c === "\n" || c === "\r") {
			// partial-json reads `[ ]` as an element it cannot parse and loses its place.
			if (s.st === FIRST_VALUE) s.st = VALUE;
			continue;
		}
		switch (s.st) {
			case VALUE:
			case FIRST_VALUE:
				if (c === "]" && s.st === FIRST_VALUE) close(s);
				else startValue(s, c);
				break;
			case FIRST_KEY:
			case KEY:
				if (c === "}" && s.st === FIRST_KEY) close(s);
				else if (c === '"') openString(s, true);
				else s.stock = true;
				break;
			case COLON:
				if (c === ":") s.st = VALUE;
				else s.stock = true;
				break;
			case AFTER: {
				const top = s.stack[s.stack.length - 1];
				if (c === ",") s.st = top.arr ? VALUE : KEY;
				else if (c === (top.arr ? "]" : "}")) close(s);
				else s.stock = true;
				break;
			}
			default: // DONE: only whitespace may follow
				s.stock = true;
		}
	}
}

function startValue(s, c) {
	if (s.stack.length === 0 && c !== "{" && c !== "[") return void (s.stock = true);
	if (c === "{" || c === "[") {
		s.stack.push({ arr: c === "[", key: undefined, count: 0 });
		s.st = c === "[" ? FIRST_VALUE : FIRST_KEY;
	} else if (c === '"') openString(s, false);
	else if (c === "t" || c === "f" || c === "n") {
		s.literal = (c === "t" ? "true" : c === "f" ? "false" : "null").slice(1);
		s.st = LITERAL;
	} else if (c === "-" || (c >= "0" && c <= "9")) {
		s.num = c;
		s.st = NUMBER;
	} else s.stock = true;
}

function openString(s, isKey) {
	s.st = STRING;
	s.isKey = isKey;
	s.esc = 0;
	s.raw = "";
	s.decoded = "";
	s.stringId++;
}

function close(s) {
	s.stack.pop();
	if (s.stack.length === 0) s.st = DONE;
	else valueDone(s);
}

function valueDone(s) {
	const top = s.stack[s.stack.length - 1];
	if (top.arr) top.count++;
	s.st = AFTER;
}

/** Consumes string text from `chunk[i]`; returns where it stopped. */
function string(s, chunk, i) {
	const start = i;
	const n = chunk.length;
	while (i < n) {
		if (s.esc === 1) {
			const c = chunk[i++];
			if (c === "u") s.esc = 2;
			else if ('"\\/bfnrt'.includes(c)) s.esc = 0;
			else return (s.stock = true), n;
		} else if (s.esc > 1) {
			if (!HEX.test(chunk[i++])) return (s.stock = true), n;
			s.esc = s.esc === 5 ? 0 : s.esc + 1;
		} else {
			PLAIN.lastIndex = i;
			PLAIN.test(chunk);
			i = PLAIN.lastIndex;
			if (i >= n) break;
			const c = chunk[i];
			if (c === "\\") {
				s.esc = 1;
				i++;
			} else if (c === '"') {
				s.raw += chunk.slice(start, i);
				stringDone(s);
				return i + 1;
			} else return (s.stock = true), n; // a raw control character: stock repairs it
		}
	}
	s.raw += chunk.slice(start, i);
	return i;
}

function stringDone(s) {
	if (s.isKey) {
		const key = JSON.parse(`"${s.raw}"`);
		// partial-json assigns keys with `obj[key] = value`; this one would set the prototype.
		if (key === "__proto__") s.stock = true;
		s.stack[s.stack.length - 1].key = key;
		s.st = COLON;
	} else valueDone(s);
	s.raw = "";
	s.decoded = "";
}

/**
 * Decodes what stock shows of the open string: partial-json trims the whole text, which drops
 * trailing whitespace inside it, and leaves out an escape that is not complete yet.
 */
function commit(s) {
	const cut = s.esc === 0 ? s.raw.trimEnd().length : s.raw.length - (s.esc === 1 ? 1 : s.esc);
	if (cut === 0) return;
	s.decoded += JSON.parse(`"${s.raw.slice(0, cut)}"`);
	s.raw = s.raw.slice(cut);
}

function leaf(value, path) {
	for (const k of path) {
		if (typeof value !== "object" || value === null || !Object.hasOwn(value, k)) return undefined;
		value = value[k];
	}
	return value;
}

/** A fresh copy of `value` with the string at `path` (from `depth`) set to `str`. */
function rebuild(value, path, depth, str) {
	if (depth === path.length) return str;
	const k = path[depth];
	if (Array.isArray(value)) {
		const out = new Array(value.length);
		for (let i = 0; i < value.length; i++) out[i] = i === k ? rebuild(value[i], path, depth + 1, str) : copy(value[i]);
		return out;
	}
	const out = {};
	for (const key of Object.keys(value)) out[key] = key === k ? rebuild(value[key], path, depth + 1, str) : copy(value[key]);
	return out;
}

function copy(value) {
	if (typeof value !== "object" || value === null) return value;
	if (Array.isArray(value)) return value.map(copy);
	const out = {};
	for (const key of Object.keys(value)) out[key] = copy(value[key]);
	return out;
}
