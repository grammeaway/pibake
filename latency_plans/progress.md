# Latency plans: progress

Live log for `README.md`'s plans: status, decisions, and the probe round's findings.

## Status

| Plan | Status | Notes |
|---|---|---|
| n1 idle keep-alive | implemented; owner's live check open | Pass `keep_alive_http`, opt-in (O7 revised), 120 s. E2E: two RPC prompts 5 s apart, 1 connection against stock Pi's 2. Open: step 2's live connect log and a day of use without `other side closed` (n1 → Owner checks). Step 1: `api.anthropic.com` keeps an idle connection > 20 min; the other `models.json` origins were unreachable (2026-10-20: both OVH hosts time out on connect, the two `127.0.0.1` servers are not running), so not probed |
| n2 first-turn prewarm | implemented; gate open (idle machine, `--record n2_prewarm`) | Pass `prewarm_first_turn`, default on (O8). `first_turn_ms` −16 % on `sea_codecache`, −23 % on `sea_trained` (indicative); the plan's "at least half" is not met: most of turn 1 is lazy compilation (n2 → Probe) |

## Decisions

| Plan | Decision | Taken |
|---|---|---|
| n1 (O7) | A pibake pass, default on: `keep_alive_http`, `keepAliveTimeout` 120 s | 2026-10-20, owner |
| n1 (O7, revised) | `keep_alive_http` is opt-in: out of `DEFAULT_PASSES`, listed in the README. Every other default pass keeps stock Pi's behaviour; this one changes how long every origin's idle connections live (extensions' included), and origins that close sooner without a `Keep-Alive` hint pay an `other side closed` retry. Defaults don't reconfigure a user's setup | 2026-10-20, owner |
| n2 (O8) | A pibake pass, default on: `prewarm_first_turn`. The owner's rule: invasive passes are opt-in, the rest default on. This one changes no setting, timeout or connection; it evaluates the same modules and creates the same dispatcher, sooner. Its costs: ~130 M instructions (+5 %) and ~1.4 MiB in a process that never prompts | 2026-10-20, owner's rule; ranking by the agent |
| Outside pibake | The owner allowed changing the live `statusbar.js`; changed (README → Outside pibake) | 2026-10-20, owner |

## n1: idle keep-alive (2026-10-20)

- **Pass `keep_alive_http`** (`src/passes/keep_alive_http.rs`), opt-in through `[build] passes` (O7, revised; first shipped in `DEFAULT_PASSES` after `lazy_http_dispatcher`). The E2E test lists it after the defaults. It matches Pi 1.3.0's whole `EnvHttpProxyAgent` options object and adds `keepAliveTimeout: 120000` on its last line, so line numbers stay. esbuild prints it as `12e4`.
- **Deviation: `lazy_http_dispatcher` accepts the keep-alive edit.** It matches `configureHttpDispatcher`'s whole text, so it now takes either stock's or `keep_alive_http`'s version. Either pass order works, and either pass works alone.
- **Deviation: the fake provider's keep-alive variant** (`FakeProvider::keep_alive`) answers with chunked framing and serves each connection until the client closes; it counts accepted connections. The default stays `Connection: close`, so `bench` does not change.
- **The E2E test's "fails without the change"** is stock `pi` in the same test: 2 connections for the 5 s gap, against 1 for the bundle. A pass-off bundle would cost a second build for the same evidence.
- **Step 1:** the OVH origins in `models.json` time out on connect from here, and the local `bonsai_*` servers were not running; their idle timeouts are unknown. A server that closes earlier than 120 s without a `Keep-Alive` hint can make a request fail with `other side closed`, which Pi retries.
- **Checks:** `cargo fmt --check`, clippy, `cargo test` and `PIBAKE_E2E=1 cargo test` green; `pibake verify` of the live build (`target/n1/on`, `pi-n1`) against stock: same (13 tools, 82 commands, 295-line system prompt).
- **No `bench` numbers.** The fake provider closes every connection in `bench`, so nothing there moves.

### Owner checks (open)

A new build for these needs `keep_alive_http` in `[build] passes`; `target/n1/on` already has it.

1. **Live connect log (step 2).** Run each of these, send two prompts about 10 s apart, then quit:

   ```sh
   cd ~/code/pibake
   CONNLOG=/tmp/n1_off.log NODE_OPTIONS="--require $PWD/target/n1/connlog.cjs" target/p8/on/target/pibake/bin/pi-p8
   CONNLOG=/tmp/n1_on.log  NODE_OPTIONS="--require $PWD/target/n1/connlog.cjs" target/n1/on/target/pibake/bin/pi-n1
   grep -c ':443' /tmp/n1_off.log /tmp/n1_on.log
   ```

   Expected: 2 connects to the provider without the pass, 1 with it. The hook loads on the SEA (checked).
2. **A day of use** of a build with the pass, without `other side closed` errors.

## n2: first-turn prewarm (2026-10-20)

### Probe

Indicative: the laptop on battery, `powersave` governor with EPP `power`. Scripts, wrappers and raw data in `target/n2/` (not committed).

- **Deviation: step 6's metric came first.** The probe needed turn 1 after a pause, which `bench` had no scenario for. `first_turn` (`first_turn_ms`): RPC, `--no-session`, the `hello` script, prompt sent 1 s after the `get_state` response, timed to the first `message_update`; the turn must end in the scripted answer. Its own scenario, so `long_session` does not change.
- **Hand patch** (`target/n2/probe`, a copy of `target/n1/on` with the stage's Pi sources edited, `pi-n2p`): `lazyApi` keeps `load` as a non-enumerable `warm`; `compat.js` sets `globalThis.__pibake_prewarm(getModel)`, which after one `setTimeout` reads `globalThis.fetch` (link 1, p5's dispatcher), then after another calls the current model's `BUILTIN_APIS` entry's `warm` (link 2, the API module). Hooks: `runRpcMode` before its final promise; `InteractiveMode.run()` after `await this.init()`. `PIBAKE_PREWARM=0` turns it off, so one binary gives both sides. Order: fetch first, because the OpenAI SDK's module init reads `Response.prototype` and would otherwise pull the dispatcher into link 2.
- **Links** (11 runs, right after ready): fetch 13–17 ms, module (`openai-completions`) 7–10 ms. Both under the plan's ~25 ms.
- **Turn 1 is bimodal on this machine**, in instructions too, so not only clock: `perf record -e instructions:u -c 100000` per run, prompt to first update: off 141 or 310–360 M, on 112–115 or 285–306 M. Batches land in one mode or the other; the cause was not found (`--no-flush-bytecode` did not change it).

  | `first_turn_ms`, median [IQR] | off | on |
  |---|---|---|
  | `sea_codecache`, 10 runs | 327.5 [323.8–329.7] (`pi-n1` 334.7) | 289.8 [285.6–291.7] |
  | `sea_codecache`, 6 runs, other batch | 96.6 [94.6–248.7] | 70.5 [67.5–89.9] |
  | `sea_trained`, 6 runs | 153.4 [69.2–233.5] | 65.3 [63.4–72.2] |
  | plain `node` bundle (compile cache from earlier runs), 5 runs | 109.9 [107.4–134.5] | |

  So: −40–60 ms in the slow mode (−12–18 %), −25 ms in the fast one (−25 %). The plan's "at least half" is not met on `sea_codecache`.
- **A warm turn after the same 1 s pause** is 25–30 ms, against 6–8 ms with no pause: the idle clock-down is ×3–4 here.
- **Where turn 1 goes** (`build --profiling`, `perf record -k mono`, windowed to the turn; off, slow mode, 315 ms): `CompileLazy` 69 % inclusive; the SDK's module init 43 %, of which p5's dispatcher (npm undici's evaluation) 26 %; `makeRequest` → `fetch` 11 %; `declareToolChanges` 2.5 %. With the prewarm (fast mode, 98 ms): `CompileLazy` 66 %. **The rest of the gap is lazy compilation of the request path**: `sea_codecache`'s cache holds only what `--build-sea` compiles eagerly. The plain bundle's compile cache, filled by earlier runs, gets turn 1 to ~110 ms without any prewarm; `sea_trained` (trained on a print turn) plus the prewarm gets it to 65 ms.
- **Not done:** the interactive pty measurement (Enter to the provider's first byte). The hook is the same function after the same kind of ready; RPC covers the code path.

### Side effects (step 2)

- **Dispatcher (link 1):** stock Pi creates it at startup; p5 defers it to the first use of a fetch global; the prewarm brings it back to right after ready, between the two. p5's proxy-variable snapshot (taken at `configureHttpDispatcher`) still applies. Interactive mode online already materialises it after `init()` (`checkForNewPiVersion`), so there link 1 is a no-op.
- **API modules (link 2):** top-level code of `pi-ai/dist/api/anthropic-messages.js` and `openai-completions.js`: constants, a `PiAnthropic` class, `Set`s and `Map`s. `@anthropic-ai/sdk` 0.129.0: a `FinalizationRegistry` (`internal/request-signal.mjs`). `openai`: a `Symbol.for` `WeakMap` registry on `globalThis` (`internal/provider.mjs`, non-enumerable) and `Response.prototype` (`auth/workload-identity-auth.mjs`), read after the dispatcher has installed undici's globals, as in stock. No env reads, timers or I/O at module level. Stock Pi evaluates the same modules on the first request; only the time moves.
- **Extensions:** the only live one touching fetch globals is `pi-web-access`, which wraps `globalThis.fetch` on its first proxied tool call (`installGlobalProxyFetch`), wrapping whatever is there; same as against stock, where the dispatcher exists from startup. None depends on the provider modules being late.
- **Errors:** a module init that throws would throw in idle time. esbuild's `__esm` marks a module initialised before running it, so the first request would then see a half-initialised module instead of the error. None of these modules throws at init; a pass would catch and ignore the warm-up's rejection, as the probe does.
- **What moves in `bench`:** ready does not. Whole-process counters may: `startup` (RPC) closes stdin right after `get_state`, so the links run before exit. Measured (5 runs, `--perf-stat`): `startup_minstr` 2450 → 2581 M (+5 %), `startup_ms` and `startup_cpu_ms` flat, peak RSS +1.4 MiB. A real client pays this once, before its first prompt.

### Implementation

- **Pass `prewarm_first_turn`** (`src/passes/prewarm_first_turn.rs`), in `DEFAULT_PASSES` after `lazy_http_dispatcher`. Eight text edits over five files, each matched whole and exactly once, each kept on its line: `lazyApi` returns its object with a non-enumerable `warm` (its loader); compat exports `pibakePrewarm(getModel)` next to `BUILTIN_APIS`; `runRpcMode` calls it before its final promise, `InteractiveMode.run()` after `init()`, and `AgentSession._emitModelSelect` on a model change (`set`, `cycle`). Two unref'd `setTimeout`s: `void globalThis.fetch`, then the model's `BUILTIN_APIS` entry's `warm()`, its rejection ignored (an API `trim_apis` dropped rejects). A model with an extension-registered API warms nothing.
- **Works alone:** without `lazy_http_dispatcher` the `fetch` read is a no-op. `PI_STARTUP_BENCHMARK` calls `init()`, not `run()`, so the interactive bench never runs it; print mode has no hook.
- **Deviation: the E2E test extends p5's** (`tests/lazy_http_dispatcher.rs`) instead of a new file: the same proxy and stalling-provider cases, now also over RPC with a 1 s pause before the prompt. The request bodies of bundle and stock are compared with Pi's package directory masked: the system prompt names it, and the plain bundle runs from its stage.
- **Deviation: the model-change hook** is in the plan's Change but not in its steps; it is one line in `_emitModelSelect`, which only user actions reach.
- **Checks:** `cargo fmt --check`, clippy, `cargo test` and `PIBAKE_E2E=1 cargo test` green; `pibake verify` of `target/n2/on` (`pi-n2`) against stock: same (13 tools, 82 commands, 295-line system prompt).
- **Bench** (step 6; indicative, battery, load 2–3; `target/n2/bench_*.txt`). `--runs 10 --perf-stat`:

  | | `sea_codecache` off | on | `sea_trained` off | on |
  |---|---|---|---|---|
  | `first_turn_ms` | 318.6 [219.4–340.9] | 266.5 [253.1–273.2] | 155.5 [98.4–217.6] | 120.2 [72.7–203.0] |
  | `startup_minstr` | 2438.2 | 2568.8 | 1927.6 | 1956.0 |
  | `interactive_minstr` | 3221.6 | 3205.0 | 2554.7 | 2553.8 |
  | `startup_ms`, `ready_ms` | 658, 956 | 637, 839 | 573, 661 | 563, 769 |

  Startup and ready wall times swing ±15 % between runs here; their instruction counts show what moved: RPC startup +131 M (+5.4 %) on `sea_codecache`, +28 M on `sea_trained` (its cache holds the module code), as the links now run before the bench closes stdin. The other scenarios, `--runs 3 --perf-stat` on `sea_codecache`: instructions within ±1 % (`big_write` 5516 → 5515 M, `long_answer` 10561 → 10600, `tui_write` 16261 → 16302, `big_output` 4792 → 4746, `long_session` 7691 → 7664).
- **Completion:** "`first_turn_ms` falls by at least half" is not met (−16 %, −23 %); "no link over ~25 ms" is (13–17 ms, 7–10 ms). The gate (step 7) is open: `bench --runs 20 --perf-stat --record n2_prewarm` on an idle machine. Candidate for the rest of the gap: put the first turn's code in the default backend's code cache (README Candidates).

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
