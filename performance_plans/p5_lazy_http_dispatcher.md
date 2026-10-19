# P5: Create the HTTP dispatcher on first use

## Finding

- **Pi pays for undici before it has anything to fetch.** `main()` calls `configureHttpDispatcher()` (Pi 1.3.0 `dist/core/http-dispatcher.js`) before parsing its arguments, and again once settings are read. Each call:
  1. loads npm undici;
  2. creates an `EnvHttpProxyAgent` and makes it the global dispatcher;
  3. calls `undici.install()`, which replaces `fetch`, `Headers`, `Request`, `Response`, `FormData`, `WebSocket`, `EventSource` and the event classes on `globalThis`.
- **pibake already wraps undici as a lazy module**, an object with a getter per export (l10). The first call's `undici.EnvHttpProxyAgent` defeats that.
- **Cost in plain node:** loading undici and creating the agent takes ~60 ms with Node's compile cache, ~95 ms without.
- **The probe** (`progress.md` → Smaller costs): the trained SEA (p4) with `configureHttpDispatcher` returning at once, against the same build without the change, 5 runs:

  | | as is | no dispatcher |
  |---|---|---|
  | RPC startup | 326 ms | 299 ms |
  | `startup_minstr` | 2376 M | 2173 M |
  | interactive ready | 365 ms | 340 ms |
  | startup peak RSS | 168.5 MiB | 163.1 MiB |
  | print turn (fetches at once) | 392 ms | 396 ms |

  RPC and interactive startup make no request; offline (`PI_OFFLINE`) they never do. An interactive user's first request comes after they have typed a prompt.

## Change

Keep the settings, defer the work. `configureHttpDispatcher(timeoutMs)` validates and records the timeout as today, then:

- **before the first use**, it installs configurable getters on `globalThis` for the names `undici.install()` sets (`undici`'s list, read at bundle time as the lazy wrapper already does). The first read of any of them materialises the dispatcher: stock's body with the recorded timeout. That replaces the getters with the real values;
- **after it**, it runs stock's body directly, as a second stock call does.

Code that imports undici's exports directly goes through the lazy wrapper. If it touches a dispatcher-dependent export first, it would get undici's default global dispatcher. Step 1 lists every importer of undici in the bundle, and the pass adds a materialising call to the wrapper's getters where one exists.

Where this lives is the owner's call (O5): a pibake pass (`lazy_http_dispatcher`, matched by exact text like l11's), or an upstream PR to Pi that stock Pi benefits from too.

## Steps

1. **Map the users:** every `undici` import, every `globalThis.fetch` capture at module load (e.g. `originalGlobalFetch`), and `Symbol.for("undici.globalDispatcher.1")` readers in the bundle. Write down which would see the dispatcher before materialisation, and how the change covers each.
2. **O5:** take the map and the probe numbers to the owner.
3. **Implement** (the pass, or the PR with a pibake check that the shape is the expected one).
4. **Tests:**
   - unit tests of the rewrite;
   - an E2E test where the fake provider receives a request through a proxy setting (`HTTP_PROXY` to a second fake server) and with a short `httpIdleTimeout`. Both behave as stock: the request goes through the proxy, and the timeout fires;
   - `verify` same.
5. **Measure:** `bench --runs 10 --perf-stat` on the default backend. Expect about −25 ms on RPC startup and on ready, and no change to the big write.

## Completion

- [x] Proxy and timeout behaviour is stock's, by test.
- [x] `startup_minstr` and `interactive_minstr` drop (−2.5 %, −2.1 %); the big write's instructions are flat, its peak RSS +2–5 MiB (`progress.md` → p5).
- [x] `cargo fmt --check`, clippy and every test green, E2E included.

## Non-goals

- Deferring other startup I/O (`LoadCACertificates`, 1.5 %). Measure it separately first.
- Changing undici's or Pi's network behaviour in any way a request could observe.
