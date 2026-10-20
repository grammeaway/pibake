// pibake:http-dispatcher: Pi's `configureHttpDispatcher`, deferred to the first use of what it
// sets up (pass lazy_http_dispatcher, src/passes/lazy_http_dispatcher.rs): a read or write of a
// global undici's `install()` replaces, or the first load of undici through the bundle's lazy
// wrapper. Until then a call only records its arguments, so the dispatcher made then is the one
// stock's last call made.

// What `EnvHttpProxyAgent` reads from the environment when it is created.
const PROXY_VARIABLES = ["http_proxy", "HTTP_PROXY", "https_proxy", "HTTPS_PROXY"];

let pending;
let replaced;
let done = false;

/** `configure(timeoutMs)` now if the dispatcher exists, else on first use. */
export function deferHttpDispatcher(configure, timeoutMs, globals) {
	if (done) return configure(timeoutMs);
	pending = { configure, timeoutMs, proxy: PROXY_VARIABLES.map((k) => process.env[k]) };
	if (replaced) return;
	const descriptors = globals.map((name) => [name, Object.getOwnPropertyDescriptor(globalThis, name)]);
	if (descriptors.some(([, d]) => d && !d.configurable)) return materializeHttpDispatcher();
	replaced = descriptors.map(([name, d]) => {
		const get = () => (materializeHttpDispatcher(), globalThis[name]);
		const set = (value) => {
			materializeHttpDispatcher();
			globalThis[name] = value;
		};
		Object.defineProperty(globalThis, name, { configurable: true, enumerable: d?.enumerable ?? false, get, set });
		return { name, d, get };
	});
}

export function materializeHttpDispatcher() {
	if (done || !pending) return;
	done = true;
	for (const { name, d, get } of replaced ?? []) {
		// A global redefined since (`Object.defineProperty`) keeps its new definition.
		if (Object.getOwnPropertyDescriptor(globalThis, name)?.get !== get) continue;
		if (d) Object.defineProperty(globalThis, name, d);
		else delete globalThis[name];
	}
	const now = PROXY_VARIABLES.map((k) => process.env[k]);
	setProxy(pending.proxy);
	try {
		pending.configure(pending.timeoutMs);
	} finally {
		setProxy(now);
	}
}

function setProxy(values) {
	PROXY_VARIABLES.forEach((k, i) => {
		if (values[i] === undefined) delete process.env[k];
		else process.env[k] = values[i];
	});
}
