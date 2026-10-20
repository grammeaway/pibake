# Latency plans: progress

Live log for `README.md`'s plans: status, decisions, and the probe round's findings.

## Status

| Plan | Status | Notes |
|---|---|---|
| n1 idle keep-alive | implemented; owner's live check open | Pass `keep_alive_http`, default on, 120 s. E2E: two RPC prompts 5 s apart, 1 connection against stock Pi's 2. Open: step 2's live connect log and a day of use without `other side closed` (n1 → Owner checks). Step 1: `api.anthropic.com` keeps an idle connection > 20 min; the other `models.json` origins were unreachable (2026-10-20: both OVH hosts time out on connect, the two `127.0.0.1` servers are not running), so not probed |
| n2 first-turn prewarm | planned | O8 open |

## Decisions

| Plan | Decision | Taken |
|---|---|---|
| n1 (O7) | A pibake pass, default on: `keep_alive_http`, `keepAliveTimeout` 120 s | 2026-10-20, owner |
| Outside pibake | The owner allowed changing the live `statusbar.js`; changed (README → Outside pibake) | 2026-10-20, owner |

## n1: idle keep-alive (2026-10-20)

- **Pass `keep_alive_http`** (`src/passes/keep_alive_http.rs`), in `DEFAULT_PASSES` after `lazy_http_dispatcher`. It matches Pi 1.3.0's whole `EnvHttpProxyAgent` options object and adds `keepAliveTimeout: 120000` on its last line, so line numbers stay. esbuild prints it as `12e4`.
- **Deviation: `lazy_http_dispatcher` accepts the keep-alive edit.** It matches `configureHttpDispatcher`'s whole text, so it now takes either stock's or `keep_alive_http`'s version. Either pass order works, and either pass works alone.
- **Deviation: the fake provider's keep-alive variant** (`FakeProvider::keep_alive`) answers with chunked framing and serves each connection until the client closes; it counts accepted connections. The default stays `Connection: close`, so `bench` does not change.
- **The E2E test's "fails without the change"** is stock `pi` in the same test: 2 connections for the 5 s gap, against 1 for the bundle. A pass-off bundle would cost a second build for the same evidence.
- **Step 1:** the OVH origins in `models.json` time out on connect from here, and the local `bonsai_*` servers were not running; their idle timeouts are unknown. A server that closes earlier than 120 s without a `Keep-Alive` hint can make a request fail with `other side closed`, which Pi retries.
- **Checks:** `cargo fmt --check`, clippy, `cargo test` and `PIBAKE_E2E=1 cargo test` green; `pibake verify` of the live build (`target/n1/on`, `pi-n1`) against stock: same (13 tools, 82 commands, 295-line system prompt).
- **No `bench` numbers.** The fake provider closes every connection in `bench`, so nothing there moves.

### Owner checks (open)

1. **Live connect log (step 2).** Run each of these, send two prompts about 10 s apart, then quit:

   ```sh
   cd ~/code/pibake
   CONNLOG=/tmp/n1_off.log NODE_OPTIONS="--require $PWD/target/n1/connlog.cjs" target/p8/on/target/pibake/bin/pi-p8
   CONNLOG=/tmp/n1_on.log  NODE_OPTIONS="--require $PWD/target/n1/connlog.cjs" target/n1/on/target/pibake/bin/pi-n1
   grep -c ':443' /tmp/n1_off.log /tmp/n1_on.log
   ```

   Expected: 2 connects to the provider without the pass, 1 with it. The hook loads on the SEA (checked).
2. **A day of use** of a build with the pass, without `other side closed` errors.

## Probe round (2026-10-20)

All runs are **indicative**: a laptop on battery with the power-saver profile (idle at 800 MHz, bursts to 4.2 GHz), load 0.4–1, 1–3 runs. Wall times came out 1.3–1.7× the p8 recording (RPC startup 678 against 402 ms), while `instructions:u` matched it. So only ratios and instruction counts are compared with recorded numbers. Executable: `target/p8/on/.../pi-p8` (live setup, `sea_codecache`, every default pass), rebuilt with `build --profiling` for `perf`. Scripts and raw data are in `target/n1/` (not committed).

### Tooling

- **`perf record -g` fails after a reboot** (`perf_event_paranoid` 2): kernel callchains are refused. `--call-graph fp --user-callchains` works without root.
- **`NODE_OPTIONS=--cpu-prof` works on a SEA, but distorts startup**: +2.5 G instructions on RPC startup, as if the code cache were not used. Use it for JS shares after startup, never for startup itself. It names JS frames on the plain build, and builtins such as `JSON.stringify` count as their JS caller's self time; `perf` on Arch's stripped `node` shows them as `[unknown]`.
- **`bench --pi` splits on whitespace**, so `env NODE_OPTIONS=… pi` silently loses its options. Use a wrapper script (`target/n1/cpuwrap.sh`).
- **`perf record -o` is overwritten by `bench`'s closing `--version` run**; the scenario's profile is the `.data.old`.
- **Taps:** `target/n1/rpctap.py` timestamps every RPC line in both directions; `ptytap.py` relays a pty and times Ctrl+D to exit; `connlog.cjs` (`--require`) logs each `net.Socket#connect`. Node passes `connect` an array of normalised arguments, so the hook unwraps `a[0][0]`. Node's built-in `fetch` uses an internal `net` and is not seen; npm undici, which Pi uses, is.
- **The fake provider answers with `Connection: close`**, so connection reuse cannot be seen through `bench`.

### Floors, measured here

- `node -e 0`: 105 M instructions. `pi-p8 --version`: 413 M. `--help`: 2372 M. RPC startup: 2439 M. Pi's own runtime set-up costs ~70 M on top of loading and evaluating the bundle.
- A print turn with a 1 KiB write: 3035 M against 2439 M for RPC startup: ~600 M for the turn, request, tool and exit.
- Print-mode streaming is linear: 1, 200, 400 KiB writes cost 3035, 5516, 7905 M instructions, ~12 k per byte, ~240 k per 20-byte delta. `perf` puts the turn at ~37 % of the run's samples and its IPC is far higher than startup's: ~10 µs per delta at full clock.

### Turn latency (RPC tap, `long_session`)

Prompt sent to the first `message_update`, with the fake provider answering at once:

| | Run 1 | Run 2 |
|---|---|---|
| turn 1 | 64.1 ms | 91.1 ms |
| turns 2–3 | 8.0, 7.5 ms | 8.6, 6.7 ms |
| turns 2–40, per turn | | prompt → first token 6–8 ms; `read` done → next first token 5–8 ms; prompt → `agent_settled` 15–25 ms |

`--cpu-prof`, the first 150 ms from `runAgentLoop` on turn 1: `openai-completions.js` → `openai/client.mjs` module init 24 ms (of which `workload-identity-auth.mjs` → p5's dispatcher materialisation 16 ms, npm undici's evaluation 14 ms of that), `declareToolChanges` 9 ms (`toToolDeclaration`, `declarationsEqual`), `makeRequest` → `fetchWithAuth` 13 ms, `__require2` and `__init` (module evaluation) 18 ms self.

### Connections (n1)

- **Pi's undici and dispatcher close idle sockets after 4 s.** `target/n1/warmcold_pi.mjs`: undici 8.10.2 from the stage, `EnvHttpProxyAgent` with Pi's options and factories, `POST` without credentials to `https://api.anthropic.com/v1/messages` (401), connect times from `undici:client:beforeConnect` and `undici:client:connected`. Default: gaps of 5 and 6 s each open a new connection (55–65 ms). With `keepAliveTimeout: 60000`, both reuse it, so the option reaches Pi's pool and clients.
- curl, 3 runs: DNS 1–5 ms, TCP connect 20–24 ms, TLS done 47–63 ms. HTTP/1.1 responses carry `Connection: keep-alive` and no `Keep-Alive: timeout=`. Edge: Cloudflare NRT.
- A 401's time to first byte: 200–500 ms, which hides the handshake in single timings. Only the connect channels separate it.
- **Server-side idle timeout: over 20 minutes.** `target/n1/ka_probe.py api.anthropic.com /v1/models` (one keep-alive GET, then wait for the close): the server had not closed the connection when the probe gave up after 1200 s. So the server's timeout doesn't limit n1's value; NAT and proxies on the path may, and Pi's retry covers a socket they drop.

### Streaming frames (`long_answer`)

`--cpu-prof` from `runLoop` on: 7.2 s of non-idle samples at the probe's clock, ~853 frames.

| Inclusive | Share |
|---|---|
| `doRender` | 87 % |
| `statusbar.js` footer `render` → `getContextUsage` → `estimateProjectedContextTokens` → `estimateTokens` | 26 % (`estimateTokens` self 24 %: `JSON.stringify` of `toolsAdded` and tool-call arguments) |
| `statusbar.js` header `render` → `piVersion` (`createRequire`, `require`), `os.userInfo` | ~5 % |
| `Markdown.render` (after p8) | 27.5 %, of which `lexMarkdown` 11.6 % (the mermaid transformer's `applyMarkdownTransformers` 8.1 %), `flattenLines` 2.3 % |
| `visibleWidth` | 7.8 %, ~75 % of it `doRender`'s per-row width check (viewport rows only) |

- Pi's own footer caches `getContextUsage` (`getSessionStats`, by session id, leaf id, entry count and model); `statusbar.js` replaces it and does not cache.
- On the first answer of a session there is no assistant usage yet, so `estimateProjectedContextTokens` estimates every message, the system message's `toolsAdded` included. Later turns estimate only the messages after the last usage, but `buildSessionProjection` and `usageFromSession` still walk the whole session per frame.

### Interactive startup and exit

- `--cpu-prof` from `init` (`InteractiveMode.init`) on: 84 ms busy in the first second. `graphemeWidth` 16 ms (self, the first ICU segmenter use), `spawnSync` 8 ms (`ensureTool` → `commandExists`), highlight.js `registerLanguage` 5 ms, `getChangelogForDisplay` → `parseChangelog` 3.4 ms, autocomplete set-up 5 ms.
- **Exit** (`ptytap.py`, `big_output`, 4 runs): Ctrl+D to process exit 90–116 ms; the last output 15–33 ms before exit. `shutdown()` awaits `terminal.drainInput(1000)`, which returns after 50 ms without input. JS from the shutdown on: ~35 ms (`--cpu-prof`), ~20 ms on-CPU (`perf`). The rest is waiting.
