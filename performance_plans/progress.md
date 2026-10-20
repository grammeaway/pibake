# Performance plans: progress

Live log for `README.md`'s plans: status, decisions, and the probe round's findings.

## Status

| Plan | Status | Notes |
|---|---|---|
| p1 pty bench | done (`6cd7677`; recording later) | `first_paint_ms`, `ready_ms`, `interactive_cpu_ms`, `interactive_peak_rss_mib`; `--big-write-kib 0` skips the big write. Step 4's breakdown is under Probe round. Recorded ready: stock **2663 ms**, warm code cache **494**, snapshot **374**; interactive peak RSS 377 / 192 / 222 MiB (`bench_results/2026-10-17_p1_interactive.json`). A first attempt was discarded: the CPU was stuck at 800 MHz after a suspend |
| p2 perf probes | done (`6cd7677`) | `build --profiling`, `bench --perf-stat` (`*_minstr`), README Profiling section; breakdown under Probe round |
| p3 ASCII bundle | done | `ascii_bundle` pass, default on; `verify` same on the live setup for both backends. Recorded: peak RSS −13 MiB (code cache), −52 MiB (snapshot); code-cache startup instructions +1.4 % (deviation, below) (`bench_results/2026-10-17_p3_ascii.json`) |
| p4 trained code cache | done (`bf3a15a`) | opt-in `sea_trained`: −20 % startup instructions, −25 % ready against `sea_codecache`, +8–13 MiB; the snapshot still leads (O3: default unchanged) |
| p5 lazy HTTP dispatcher | done | O5 = pibake pass (owner); `lazy_http_dispatcher` on by default; proxy and idle timeout as stock by E2E; `verify` same. Recorded on `sea_codecache`: startup_minstr −2.5 %, RPC startup −18 ms, ready −16 ms; less than planned (below) (`bench_results/2026-10-19_p5_lazy_http_dispatcher.json`) |
| p6 lazy modules | done | `lazy_modules` on by default, not on the snapshot; 11 files, typebox dropped; `verify` same, markdown renders as stock by E2E. Recorded on `sea_codecache`: startup_minstr −15.4 %, RPC startup −45 ms, big write −37 ms; interactive ready +10 ms (below) (`bench_results/2026-10-19_p6_lazy_modules.json`) |
| p7 session bench | done | `long_answer`, `tui_write`, `big_output`, `long_session` in `bench`, `--scenarios`; profiles and ranked list below and in the README. Recorded on `sea_codecache` against stock: `long_answer` 11.5 s against 14.5, `tui_write` 2.9 s against 51.6, `big_output` 0.97 s against 4.4, a late session turn ~24 ms on both, session end RSS 313 MiB against 484 (`bench_results/2026-10-19_p7_session.json`) |
| p8 incremental Markdown | done | O6 = pibake pass, mermaid lex folded in (owner); `incremental_markdown` on by default; every frame as stock by E2E; `verify` same. Recorded on `sea_codecache`, pass off → on: `long_answer` CPU 11.55 s → 3.29 s (−71 %), instructions −82 %, peak RSS −14 MiB; other scenarios flat (`bench_results/2026-10-20_p8_markdown.json`) |

## Decisions

| Plan | Decision | Taken |
|---|---|---|
| p1 | `Screen` parses escape sequences byte by byte, so a sequence split across reads needs no carried tail (the plan said "a short tail") | 2026-10-17, agent |
| p1 | Readiness checks all text drawn up to ready for the model id, not only the final frame: Pi redraws changed lines only, so the last frame often lacks the footer | 2026-10-17, agent |
| p2 | Profiling flags per backend: code cache gets `--perf-basic-prof --interpreted-frames-native-stack`; the snapshot only `--perf-basic-prof`, since a snapshot built with both segfaults at launch (Node 26.11.1) | 2026-10-17, agent |
| p3 | `ascii_bundle` runs in `Bundler::run` on the final bundle, not in `passes::run`, so it is listed in `[build] passes` but skipped there. Its report's file is bundle-relative (`main.mjs`, `main.cjs`) | 2026-10-17, agent |
| p3 | A non-ASCII character after `\` in a regex (an identity escape, non-`u` only) is written as one escape in place of both, since `\` before the escape would make it a literal `\u` | 2026-10-17, agent |
| p5 | O5 = pibake passes, not upstream PRs: the change is a startup optimisation, not a Pi bug, and Pi's maintainers are swamped. Applies to p6's pi-tui part too | 2026-10-19, owner |
| p5 | undici is wrapped lazily on every backend while the pass is on; its first load materialises Pi's dispatcher, so a second undici copy (pi-ai's Gemini fetch) shares it as in stock | 2026-10-19, agent |
| p5 | A global redefined with `Object.defineProperty` before the first use keeps its definition; stock's `install()` then skips, as `fetch` is no longer the original | 2026-10-19, agent |
| p6 | `lazy_modules` is skipped on `sea_snapshot`: a module evaluated after the resume misses `snapshot_safety`'s initialisers (interactive crashed on `utils.js`'s segmenters), and the snapshot holds these modules evaluated anyway | 2026-10-19, agent |
| p6 | Exports no code reassigns become plain values on the stand-in once loaded; a getter cost ~25× per call | 2026-10-19, agent |
| p6 | Keep all 11 files on by default, for −15 % startup instructions in RPC and print mode at +10 ms interactive ready on `sea_codecache` | 2026-10-19, owner |
| p7 | A session turn ends at `agent_settled` with no compaction running, not at `agent_end` (the plan). `agent_settled` is Pi's "no more automatic work"; pi-observational-memory compacts after it (~98 k estimated tokens, turn ~20 on the live setup), and Pi rejects a prompt meanwhile. The bench sends it again after `compaction_end`; the compaction's CPU lands in that turn | 2026-10-19, agent |
| p7 | Per-turn CPU is the process CPU clock (`clock_getcpuclockid`), not `/proc/<pid>/stat` (the plan): its 10 ms ticks are a third of a turn. The process is the busiest one in the command's tree, so Pi under `perf stat` or `perf record` | 2026-10-19, agent |
| p7 | `session_end_rss_mib` and `session_peak_rss_mib` are the Pi process's `VmRSS` and `VmHWM` after the 40th turn; `wait4`'s peak read below `VmRSS` once (it covers the wrapper and is taken at exit) | 2026-10-19, agent |
| p7 | The fake provider answers an extension's own model call with `pibake-fake-side`, and closes a turn with `pibake-fake-done` (was `Done.`), so a drawn side reply never ends a pty run. A reply after a tool checks the result (the `seq` output's last line, the read file's last line), else answers `pibake-fake-unexpected: …`, which fails the run | 2026-10-19, agent |
| p7 | `bench`'s table leaves out metrics no command has (27 rows otherwise) | 2026-10-19, agent |
| p7 | `bench` and `train` use an unrecorded fake provider. The first `p7_session` recording kept every request: pibake grew ~65 MiB per round from `long_session`'s contexts, and Pi, started by fork and exec, inherits the forking process's RSS in `ru_maxrss`. Its pty peak RSS samples rose 286 → 668 MiB from round 5 on; discarded and recorded again. pibake now stays at ~8 MiB, under any Pi's peak. Earlier recordings were unaffected (no growing scenario) | 2026-10-19, agent |
| p2 | `--perf-stat` counts `instructions:u` only, as `startup_minstr`, `interactive_minstr`, `big_write_minstr` (millions). It wraps the harness command in `perf stat -x, -o <harness>/perf_stat.csv`; `wait4` then reaps perf, whose own CPU (a few ms) lands in the time metrics | 2026-10-17, agent |

## Probe round (2026-10-17)

Everything here is **indicative**: a laptop under load 1–4 (other benches, a sub-agent), 1–5 runs. `instructions:u` is the number to trust: its spread across runs was ±0.3 %, against ±2–5 % for time. Setup: the live `~/.pi/agent` re-locked into `target/probe/` (Pi 1.3.0, Node 26.11.1; the live setup had drifted from the l10/l11 locks), every build from that one lock. Scripts and raw data stay in `target/probe/` (not committed).

### Tooling

- **Runtime perf flags reject the code cache.** `NODE_OPTIONS=--perf-basic-prof` (or `--interpreted-frames-native-stack`) on a built executable: +290–380 ms and +2.0–2.3 G instructions for `--help`, the cost of having no code cache. Baked at build time (`build --profiling`) the cost drops to +0.6 G (map writing). A control flag (`--max-semi-space-size`) costs nothing.
- **`--require` through `NODE_OPTIONS` works on a SEA** and keeps the cache (+2.5 % instructions for a probe script). That is how heap and mapping statistics were taken (`target/probe/memprobe.cjs`: `/proc/self/smaps`, `v8.getHeap*Statistics` at exit).
- **Symbols.** Arch's `nodejs` is stripped, and the SEA keeps its build ID. Arch's debuginfod serves the debug file but at about 1 MB/s for about 3.3 GB. The official nodejs.org `v26.11.1` Linux binary ships unstripped with debug info: building with it on `PATH` (same version, so the lock holds) gives named V8 and Node frames. It is a different build (static OpenSSL and ICU), so its profiles explain where time goes, and Arch builds give the numbers.
- **Profiling builds distort what they measure.** With `--perf-basic-prof`, V8 computes line ends over the 20 MB source for every logged function (`Script::InitLineEnds` 4–5 % of samples). With `--interpreted-frames-native-stack`, Node's built-in modules lose their embedded code cache and get parsed (`compileForInternalLoader` 15 %). So: plain builds (official node) for native categories, profiling builds only to name JS functions.
- **`bench --perf-stat` overhead:** +0–9 ms startup, +11 ms ready (perf's own start), CPU and RSS within noise.

### Where startup goes

`perf record -e cycles:u -g`, RPC startup, plain official-node builds; main thread unless noted (categorised from `perf script` by `target/probe/perfincl.py`):

| | `sea_codecache` | `sea_snapshot` |
|---|---|---|
| main thread share of all samples | 77 % | 73 % |
| lazy compilation (functions not in the cache) | **40 %** (parse 32 %) | 24 % |
| module evaluation (all of it, compile included) | 48 % | 0 (in the snapshot) |
| regexp construction and compile, ICU | 11 % | 4 % |
| code cache / snapshot deserialisation | 4 % / 7 % | 23 % |
| GC on the main thread | 2 % | 7 % |
| worker threads: GC, baseline compile | 11 %, 11 % | 17 %, 10 % |

- **The code cache only holds what V8 compiles eagerly**: top-level code. Every function that startup calls is compiled from source at launch. That is the 40 %.
- **The snapshot pays for deserialising its 90 MiB of heap**, and still compiles lazily whatever runs after the resume.

`PI_TIMING=1` (Pi's own phases, relative to `main()`; 8 runs):

| | stock | code cache | snapshot |
|---|---|---|---|
| `createAgentSessionRuntime` | 2352 ms | 126 ms | 150 ms |
| of which extensions (`TOTAL`) | 2271 ms | 49 ms | 48 ms |
| first extension's import (`init.ts`, 59 lines) | 151 ms | 22 ms | 22 ms |
| `interactiveMode.init` (tty) | | 51 ms (trained cache) | |

Everything else in `main()` is under 5 ms. `createAgentSessionRuntime` is ~40 % of a built executable's RPC startup; the rest before `main()` is process start, deserialisation and module evaluation.

### Trained code cache (p4)

`vm.Script#createCachedData()` taken at the *end* of a run includes every function compiled during it. Chained over three runs (RPC startup, interactive startup, a 1 KiB print turn: each run loads the previous cache and writes a new one), the cache is 7.6 MB against the build-time cold one's 2.5 MB. Prototyped first under plain `node` (a loader that `vm.Script`s the CommonJS bundle), then as a SEA: a 7-line CommonJS main that compiles the `bundle.cjs` asset with the `cache.bin` asset as `cachedData`. 5 runs, `--perf-stat`, same lock:

| | `sea_codecache` | `sea_snapshot` | trained SEA | trained SEA, ASCII bundle |
|---|---|---|---|---|
| RPC startup | 450 ms | 325 ms | 337 ms | **325 ms** |
| RPC instructions | 2934 M | 1921 M | 2239 M | 2378 M |
| interactive ready | 499 ms | 386 ms | 376 ms | **368 ms** |
| print turn (1 KiB) | 556 ms | 421 ms | 404 ms | **395 ms** |
| startup peak RSS | 181 MiB | 213 MiB | 196 MiB | **170 MiB** |
| interactive peak RSS | 192 MiB | 224 MiB | 208 MiB | **180 MiB** |

- With an ASCII bundle, the trained SEA matches the snapshot on RPC startup, beats it on interactive ready and on the print turn, and uses 43 MiB less memory, without any of the snapshot's build-time rules.
- Lazy compilation drops from 40 % to 5 % of the main thread. What remains is deserialising the 7.6 MB cache (~10 %), JS execution, and regexps.
- **Not reproducible as bytes:** two trainings gave caches of different sizes (7,639,440 vs 7,639,584 bytes). l6's "rebuild byte-identical" would not hold.
- **Snapshot plus trained cache does not work as tried:** a snapshot builder that compiles the bundle with the trained cache gets it rejected. `mksnapshot` runs V8 under other flags. Forcing `--no-flush-bytecode` there changes the flag hash too.

### Memory

Code-cache executable, RPC startup, 184 MiB peak:

| | MiB |
|---|---|
| file-backed (the executable's mapping 68: Node's text ~39, SEA main ~21, code cache ~8; libraries 15) | 83 |
| V8 heap (`heapTotal`; 36 used) | 45 |
| the bundle source, **an external two-byte string** (`external_script_source_size`) | 41 |
| other anonymous (malloc, ICU, stacks) | ~16 |

- **Why two bytes:** Node turns the SEA main into a V8 string through `StringBytes::Encode(UTF8)`. Pure ASCII becomes an external one-byte copy. Anything else above 1 MiB becomes UTF-16, because its Latin-1 fast path is capped at 1 MiB (`src/string_bytes.cc`, v26.x). So for a 20 MB bundle, Latin-1 characters cost as much as emoji. **The target is pure ASCII** (p3 corrected).
- The bundle has 1,446 non-ASCII characters on 285 lines: 248 comment-only lines, about 35 regex literals, a legal comment.
- Experiments on the code-cache backend: peak RSS **184 → 172 MiB** with an ASCII bundle (a crude escape of every non-ASCII character), **161** with `minifyWhitespace` too, **155** with full minification. `instructions:u` stays within ±1 % for all three.
- Snapshot: the same source sits on the heap as a 41 MiB two-byte string (`large_object_space`). The executable's mapping is 91 MiB resident.
- **Big write after l11:** peak RSS 229 MiB (code cache) vs 259 (snapshot). l10's "+150 MiB" gap is now ~+30.

### Smaller costs, by name

From profiling builds (JS names) cross-checked against plain ones; shares are of the main thread.

- **Regexps and ICU: 16 % (RPC) and 22 % (interactive) of the trained build's main thread.**
  - pi-tui's `dist/utils.js` module init alone: regex literals with `\p{…}` and `/v` (`zeroWidthRegex`, `rgiEmojiRegex`, …) 5 %, and an `Intl.Segmenter` at module level (ICU `Locale::createCanonical`) 3 %.
  - marked's `getRegex` (`new RegExp` with `\p{P}\p{S}`) 2 %.
  - Interactive only: `graphemeWidth`'s first `exec` 4 %.
  - RPC and print mode never use any of these.
- **undici:** Pi's `configureHttpDispatcher()` runs at the start of `main()`, loads npm undici and creates an `EnvHttpProxyAgent`. In plain node that is ~60 ms warm and ~95 ms cold. In the trained SEA, an early return there saves 27 ms of RPC startup (−8 %), 25 ms of ready, 200 M instructions and 5 MiB. The print turn is unchanged, since it fetches at once.
- `node::crypto::LoadCACertificates` at startup: ~1.5 % of the main thread.
- First extension import costs 22 ms on both backends against ~1 ms for the rest (PI_TIMING above). Not attributed yet.

### p1 step 1 findings (sub-agent)

- Pi picks interactive mode with a pty on stdin and stdout; no dialogs in the harness home.
- `ready_ms` is `renderNow()`'s frame. The highlight re-render (step 8) lands inside Pi's 150 ms wait, draws no text, and is not counted.
- Without a DA1 reply, `ready_ms` rises ~90 ms and nothing else changes.
- **`first_paint_ms` is bimodal.** Pi's first render after `ui.start()` races the instant DA1 reply. If the render loses, the first frame is `renderNow`'s, so `first_paint == ready` in about half the runs. Read `ready_ms`.

## Session probes (2026-10-17)

Indicative, load ~1. Feeds the README's native-code verdicts, p7 and p8.

- **Markdown while streaming:** pi-tui 1.3.0's `Markdown` under plain `node` (`/tmp/mdprobe/probe.mjs`, not kept), colourless theme. The text grows 24 characters at a time, with one `setText` and `render(120)` per step: 10 k chars 1.3 s CPU, 20 k 4.0 s, 40 k 16.6 s, 60 k 36.7 s.
- **Transcript frame cost:** 400 cached `Markdown` messages (17,600 lines) in a `Container`, `render(120)` with caches warm: 0.19 ms per frame (20 and 100 messages: 0.03 ms). `doRender`'s post-processing works on the visible rows only (`tui-alt-screen.js`).
- **Subprocesses at interactive startup:** a `--require` hook logging every `child_process` call. Setup: `pi-baked` (live setup's baked extensions), a throwaway agent directory, `PI_OFFLINE=1`, `PI_STARTUP_BENCHMARK=1`, a pty from `script`. Only `spawnSync fd --version` (6.4 ms) and `spawnSync rg --version` (3.3 ms) ran, both from `ensureTool`. Under plain `node` with 150 MiB of ballast: `rg` 8.8 ms, `fd` 8.2 ms, a missing `fdfind` 5.3 ms.
- **Pi-Bolt against Pi v1.3.0:** a sub-agent diffed `packages/{ai,agent,tui,coding-agent}/src` and catalogued 13 Pi-level performance changes. They are summarised in the README's verdicts and candidates. 1.3.0 already has `Markdown.cachedTokens`, a `Box` line cache and incremental write highlighting while streaming, so Pi-Bolt's numbers, taken against 1.1.0 and 1.2.0, overstate what is left.

## p3: ASCII bundle (2026-10-17)

**Recorded** on an idle machine (owner): `bench --runs 20 --perf-stat --record p3_ascii`, live setup, one lock, pass on and off per backend (`target/p3/`, not committed). The code-cache executables ran without a warm cache, so every run includes extraction, the same for on and off. The live bundle: 235 comments and 30 regex literals escaped, +2.3 KB, 0.3 s in the pass.

| | code cache on | off | snapshot on | off |
|---|---|---|---|---|
| startup peak RSS | **175.2 MiB** | 188.3 | **159.4** | 211.1 |
| interactive peak RSS | **185.9** | 197.5 | **173.0** | 221.8 |
| big write peak RSS | **219.3** | 233.1 | **210.5** | 254.5 |
| startup_minstr | 2954 | 2912 | **1814** | 1917 |
| interactive_minstr | 3260 | 3222 | **2347** | 2392 |
| big_write_minstr | **5933** | 6181 | **4808** | 4921 |
| RPC startup | 455 ms | 462 | **297** | 320 |
| ready | 509 ms | 518 | **362** | 386 |

- **Memory as planned on the code cache (−13 MiB, plan −12), and well beyond on the snapshot (−52, plan −20).** The snapshot also runs 5 % fewer startup instructions and starts 23 ms sooner, since its heap no longer holds a 41 MiB two-byte string to deserialise.
- The big write runs 4 % fewer instructions on the code cache (−2 % on the snapshot).
- **Deviation: code-cache startup instructions rise 1.4 %** (plan: within 1 %), interactive 1.2 %; times do not rise (−7 and −9 ms). Profiled with official-node builds of the same lock (`perf record -e instructions:u`, `--help`): V8 reads an external one-byte source through a buffered stream that copies each chunk to UTF-16 (`CopyChars<unsigned char, unsigned short>`, +11 M), where it reads a two-byte one in place. On the official build the net is −0.4 % on `--help`; on Arch's build +0.6 %. The cost is per lazily compiled function, so p4's trained cache should remove most of it.
- An empty `HOME` makes the code-cache executable fail at launch: its prologue extracts to the relative `.cache/pibake/…`, and `createRequire` rejects the relative path. Stock Pi starts. Found by accident while profiling; not fixed here.

## p4: trained code cache (2026-10-18)

**Recorded** on an idle machine (owner): `bench --runs 20 --perf-stat --big-write-kib 200 --record p4_trained`, live setup, one lock (`target/p4/`, not committed). `untrained` is the `sea_trained` executable with a cache made right after compiling, as `--build-sea` does: step 1's parity check. Training (three runs) takes ~10 s of a build; the cache grows from 2.5 to 7.6 MB.

| | stock | `sea_codecache` | `sea_snapshot` | untrained | `sea_trained` |
|---|---|---|---|---|---|
| RPC startup | 2618 ms | 461 | **291** | 452 | 343 |
| ready | 2692 ms | 512 | **360** | 508 | 385 |
| startup_minstr | 20379 | 2955 | **1813** | 2867 | 2369 |
| interactive_minstr | 20751 | 3260 | **2349** | 3169 | 2594 |
| big write | 47493 ms | 886 | **735** | 878 | 738 |
| big_write_minstr | 537390 | 5937 | **4815** | 5813 | 5189 |
| startup peak RSS | 364.7 MiB | 169.3 | **160.6** | 169.8 | 178.2 |
| interactive peak RSS | 378.1 | 179.6 | **174.2** | 180.4 | 187.8 |
| big write peak RSS | 697.2 | 215.8 | **212.4** | 216.1 | 228.6 |

- **The training works as planned.** Against `sea_codecache`: startup_minstr −20 % (plan −20), ready −25 % (plan −25), RPC startup −26 %, big write −17 %. Untrained is at parity (−3 % instructions), so the gain is the training's.
- **Deviation: memory rises 8–13 MiB** (plan: 170 against 181 MiB, from the pre-p3 probe). Untrained is at parity, so it scales with the cache: V8 materialises every function in it at launch, used by this launch or not, and the 7.6 MB asset's pages are touched.
- **The snapshot now leads on every startup metric and on memory**: against `sea_trained`, −15 % RPC startup, −7 % ready, −23 % startup instructions, −18 MiB. The probe ranked trained ahead on ready (368 against 386 ms); p3 has since cut the snapshot's memory by 52 MiB and its startup by 23 ms. The big write is a tie.
- **Coverage (step 4):** chaining a `--help` run onto the trained cache adds 27 KB (+0.4 %); three more runs under `PI_TIMING=1` add 11 KB (+0.1 %). Training keeps the three runs.
- **Rebuilds are byte-identical** once the loader lives in the bundle directory: the executable embeds its main's path, and a temporary directory differed per build.
- O4 (owner): training runs extensions at build time only behind the opt-in `backend = "sea_trained"`, never by default.
- O3 (owner, on these numbers): `sea_codecache` stays the default; `sea_snapshot` and `sea_trained` stay as opt-ins.

## p5: lazy HTTP dispatcher (2026-10-19)

**Step 1, the map** (live setup bundle, Pi 1.3.0, undici 8.10.2):

| User | When | Covered by |
|---|---|---|
| Pi's `http-dispatcher.js`: undici import, `originalGlobalFetch` captured at module load | module init | unchanged: the capture runs before any getter exists |
| `fetch`, `globalThis.fetch`, `WebSocket`, … read by provider SDKs, `fetchWithRetry`, the MCP client | first request | the read materialises, then returns the real value |
| pi-web-access `installGlobalProxyFetch` (wraps `globalThis.fetch`) | a proxied search only | the read materialises, so it wraps undici's fetch as in stock |
| A second undici, `node_modules/undici` (pi-ai's Gemini fetch, dynamic import) | first Gemini request | the lazy wrapper's load materialises: both copies share `Symbol.for("undici.globalDispatcher.*")` |
| Proxy variables | read by `EnvHttpProxyAgent` when created | the last call's values are restored around the deferred call; `no_proxy` is read per request by undici |

**Trace** (a logging build, live setup): RPC startup and offline interactive startup never materialise. Online interactive does, from `checkForNewPiVersion` in `InteractiveMode.run()`, after `init()` drew the ready frame. Print mode materialises on its request.

**Tests:** `tests/lazy_http_dispatcher.rs`. The module's cases under `node`; a bundle against stock Pi on a print turn through a `CONNECT` proxy from `httpProxy` (same tunnels) and one cut by `httpIdleTimeoutMs: 1000` on a stalled stream (same output, `terminated`, 1–20 s). With `configure` removed from the module, the proxy test fails.

**Recorded** on an idle machine (owner): `target/p5/bench.sh`, `bench --runs 10 --perf-stat --record p5_lazy_http_dispatcher`, live setup re-locked (Node 26.12.0 now, so not comparable to p4's table), one lock for all four (`target/p5/`, not committed).

| | `sea_codecache` on | off | `sea_trained` on | off |
|---|---|---|---|---|
| RPC startup | **467.0 ms** | 484.7 | **355.0** | 360.2 |
| ready | **522.2 ms** | 538.3 | **396.4** | 412.3 |
| startup_minstr | **2882** | 2956 | **2328** | 2368 |
| interactive_minstr | **3192** | 3260 | **2549** | 2587 |
| big_write_minstr | 5956 | **5941** | **5170** | 5190 |
| big write | 932.8 ms | 931.0 | 788.3 | 775.3 |
| startup peak RSS | 171.7 MiB | 171.7 | 176.0 | 176.3 |
| interactive peak RSS | **181.7** | 183.0 | 185.6 | 185.7 |
| big write peak RSS | 218.3 | **216.0** | 232.9 | **228.3** |

- **Startup gains as planned in kind, smaller in size:** −18 ms RPC startup and −16 ms ready on the default backend (plan −25), −75 M startup instructions (probe −200 M, on the trained SEA). On `sea_trained`: −5 ms, −16 ms ready, −40 M.
- **Deviation: the gap to the probe is not attributed.** A hook at exit confirmed the dispatcher is still deferred and npm undici unevaluated after RPC startup. The probe returned early from `configureHttpDispatcher`, so its print turn used Node's built-in fetch and npm undici never ran, in training either; the trained caches here differ by only 25 KB (on 7.60 MB, off 7.62 MB), so that does not explain it on its own.
- **Deviation: the big write.** Instructions +0.25 % on the code cache (−0.4 % trained), wall time within noise; peak RSS +2.3 MiB (code cache) and +4.6 MiB (trained): undici and the dispatcher are now created mid-session, on a larger heap. Logged, not chased: below the startup gain, and p7 profiles the session.
- **Found:** Node's built-in undici (`internal/deps/undici/undici`) is still loaded at RPC startup, on and off. Pi's `http-dispatcher.js` reads `globalThis.fetch` at module load (`originalGlobalFetch`), which runs Node's lazy getter. A candidate, unmeasured.


## p6: lazy modules (2026-10-19)

Trace, side-effect review and implementation notes: `p6_lazy_modules.md` → Outcome.

**Recorded** on an idle machine (owner, load 0.3–1.0): `target/p6/bench.sh`, `bench --runs 10 --perf-stat --record p6_lazy_modules`, live setup, one lock (`target/p6/`, not committed). `A` is marked, `markdown.js` and `mermaid.js` only.

| | `sea_codecache` on | A | off | `sea_trained` on | off |
|---|---|---|---|---|---|
| RPC startup | **422.7 ms** | 456.9 | 467.3 | **317.7** | 361.1 |
| ready | 532.5 ms | 527.9 | **522.6** | 402.3 | 398.7 |
| startup_minstr | **2437** | 2774 | 2880 | **1925** | 2326 |
| interactive_minstr | 3240 | 3210 | 3193 | 2556 | 2550 |
| big_write_minstr | **5511** | 5852 | 5957 | **4779** | 5175 |
| big write | **905.4 ms** | 928.6 | 942.4 | **749.5** | 799.9 |
| startup peak RSS | **166.6 MiB** | 168.5 | 173.0 | **173.9** | 177.3 |
| interactive peak RSS | **180.2** | 179.4 | 183.1 | **185.2** | 187.4 |
| big write peak RSS | **213.5** | 215.5 | 218.7 | **224.0** | 232.6 |

- **Startup gains beyond the plan's estimate** (~10 % from the probe): −15.4 % startup instructions and −45 ms RPC startup on the default backend, −17.2 % and −43 ms on `sea_trained`, 3–6 MiB less memory. The big write gains as well (−7.5 % instructions), being a print-mode launch. Most of it is group B: A alone is −3.7 %.
- **Deviation: interactive costs a little more**, as it loads all 11 anyway. Ready +10 ms on `sea_codecache` (spreads touch), +4 ms on `sea_trained` (within spread). `interactive_minstr` is bimodal in every variant (clusters near 3190 and 3235 M, ~40 M apart, on and off alike); like for like, the cost is ~+15 M (+0.5 %). Not attributed; candidates are the stand-ins' first-load work and the getter esbuild's `__toESM` keeps between an importer and a stand-in.

## p7: session bench (2026-10-19)

**Step 1, by hand** (stock and `pi-p6-on`, fixture and live setup):

- Interactive mode submits a positional prompt at startup; Ctrl+D (`\x04`, legacy keys: the bench does not answer the keyboard protocol query) on the empty editor exits 0, also right after the closing text is drawn.
- The closing texts are found in the text drawn across all frames (p1's lesson), also split across reads.
- RPC 1.3.0: `prompt` answers `{"disposition":"started"}`; `message_end` carries the final assistant message; `agent_settled` ends the work. The session file goes to the harness agent directory.
- **The live setup compacts mid-session.** pi-observational-memory triggers compaction after `agent_settled` once its estimate passes ~98 k tokens (turn 19–20). It is deferred if the next prompt wins the race, and otherwise rejects that prompt. See Decisions. Its own model calls and every other extension's take the side reply; no turn count broke.

**Recorded** on an idle machine (owner): `target/p7/bench.sh`, `bench --runs 10 --perf-stat --scenarios long_answer,tui_write,big_output,long_session --record p7_session`, live setup, one lock (`target/p7/`, not committed). Medians [p25–p75]:

| Metric | stock | `sea_codecache` |
|---|---|---|
| `long_answer_cpu_ms` | 14538 [14446–14587] | 11515 [11468–11542] |
| `long_answer_peak_rss_mib` | 457 [456–459] | 267 [264–269] |
| `long_answer_minstr` | 92649 | 77437 |
| `tui_write_cpu_ms` | 51571 [51335–51744] | 2923 [2903–2936] |
| `tui_write_peak_rss_mib` | 698 [631–727] | 318 [318–319] |
| `tui_write_minstr` | 548118 | 16320 |
| `big_output_cpu_ms` | 4413 [4359–4426] | 972 [961–979] |
| `big_output_peak_rss_mib` | 420 [419–423] | 223 [222–230] |
| `big_output_minstr` | 22231 | 4798 |
| `session_early_turn_cpu_ms` | 35.9 [32.4–37.1] | 32.6 [32.0–35.1] |
| `session_late_turn_cpu_ms` | 21.6 [19.7–22.6] | 24.3 [21.9–26.7] |
| `session_end_rss_mib` | 484 [480–488] | 313 [303–318] |
| `session_minstr` | 25067 | 7704 |

- `long_answer` is p8's baseline: 11.5 s of CPU on the default backend for a 20 KiB answer, 85 % of stock's instructions. The backend helps it least: the cost is Pi's own render, not loading.
- `tui_write` and `big_output` gain most from the default backend (l11's streaming JSON and startup); stock's `tui_write` is l11's quadratic parse.
- Session turns cost the same on both; the backend's gain there is startup and memory.
- Two earlier recordings were discarded: the first for the fake provider's growth (Decisions), the second was not saved, since `--record` does not overwrite.

**Profiles** (step 4): `target/p7/profile.sh`, `perf record -F 999 -e cycles:u -g`, one run per scenario, live setup, one lock. Stock as `node --perf-basic-prof --interpreted-frames-native-stack cli.js`, the default backend as `build --profiling` (`pi-p7-profiling`). Plus `--cpu-prof` of stock for `long_session` and `long_answer`, from the first sample under the agent loop on (`target/p7/cpuprof.py`). Indicative: load 2–4, perf's own CPU included. Shares are of all samples (`perf`) or of non-idle samples after startup (`--cpu-prof`); "incl." is inclusive.

| Scenario | stock | default backend |
|---|---|---|
| `long_answer` | `Markdown.render` 81 % incl. after startup: marked `lex` 26 %, highlight.js 18 %, `wrapTextWithAnsi` 20 %; the mermaid transformer (`applyMarkdownTransformers`) 13 %; the footer's `estimateTokens` 2 % | the same: `Markdown.render` 53 % incl. of the process, `wrapTextWithAnsi` 15 %, `lex` 10 %, highlight 7 %, `getContextUsage` 3 % |
| `tui_write` | `parseStreamingJson` 80 % incl. (l11's target); jiti 3 %; the write call's render 2 % | `renderCall` → `formatWriteCall` per delta **44 %** incl.; startup ~11 %; the SSE parse 4 % |
| `big_output` | jiti (extension transpiling at startup) 34 % | startup ~30 %; the TUI render ~5 %; the output accumulator (`append`, `handleData`) 2 % |
| `long_session` | jiti ~30 %. After startup: the OpenAI SDK's body encoding (`FallbackEncoder`, `JSON.stringify` of the request) **11 %** self, tool declaration diff per turn (`declareToolChanges` → `getToolStateChanges`, `toToolDeclaration`, `declarationsEqual`) **11 %** incl., `structuredClone` (pi-btw's `context` handler) 4 %, the lone-surrogate regex 2 %, RPC `serializeJsonLine` 1.4 %, `estimateTokens` 0.9 % | startup ~17 %; `declareToolChanges` 5 % incl., `buildRequest` 4 % (`FallbackEncoder` 3.6 %), `structuredClone` 2.3 %, `transformContext` 2.2 % |

- **`long_answer` is p8's.** The answer's own Markdown is ~80 % of its CPU after startup, on both. The mermaid transformer lexes the whole text again per render, and the footer re-estimates the context's tokens per frame; p8's plan covers neither.
- **`tui_write` on the default backend is now the write call's render**, not the parse (l11): `formatWriteCall` formats the whole content so far per delta, so it is quadratic.
- **`big_output` is cheap on 1.3.0**: 8 MiB of output costs less than startup. Pi-Bolt's bash-output fixes target 1.1.0 and 1.2.0.
- **`long_session` turns do not grow over 40 turns**: late turns cost less than early ones (JIT warm-up), on the fixture with no compaction as well. Per turn, ~25–35 ms. **Memory does**: the default backend ends at ~300 MiB, stock at ~510 MiB, against ~170 MiB after startup, for 720 KiB of session content. A heap probe at exit (`target/probe/memprobe.cjs` through `NODE_OPTIONS`, default backend, live setup) against RPC startup: V8 heap 144 MiB against 44, of which new space 55 MiB reserved (14 used: the young generation grown to its cap, not live data), **large objects 34 MiB live** (0 at startup), old space 43 used (20); array buffers 16 MiB (0.2). Large objects that size, for 720 KiB of content, suggest strings of the whole context kept per turn. Not attributed further.

## p8: incremental Markdown (2026-10-20)

**Step 1, the map** (`target/p8/probe/fuzz.mjs`, not committed): pi-tui 1.3.0's own `markdownParser` (marked 18.0.11, strict strikethrough, LaTeX extensions), random documents from 17 construct families (paragraphs with every inline kind, ATX and setext headings, tight, loose, nested, task and gapped lists, empty-start items, lazy and nested quotes, open and closed fences, indented code, tables, hr, HTML blocks including `<pre>` across blank lines, `$$` and `\[` blocks open and closed, every interrupting construct, link reference definitions, CJK, emoji, tabs). Streamed in deltas of 1–30 characters, each prefix trimmed and tab-replaced as Pi does; after each, the incremental tokens (cached tokens but the last two, the rest lexed from their offset, `trimPartialClosingFences` on the result) are compared deep-equal to a full lex.

- **A fixed "last two tokens" window is not enough.** It passed ~1.06 M mixed checks, but a run over every pair of families (16×16, 150 documents each) found three kinds of reach-back: a later item joining (and loosening) a list across a blank line, lazy continuation turning a cut-off `2.` line back into paragraph text, and a pending `$$` line (not yet a `latexBlock`) closing against a much later `$$`. A single token fails sooner still: a lazy blockquote line after one `\n`, which marked appends to the previous token's `raw`.
- **The rule that holds: cut at a blank line before a block that another token follows.** Keep the cached tokens up to a `space` token at or before the second-to-last token, and lex the rest from that offset: the first re-lexed block's first line is complete, so whether it continues what precedes the blank line was settled when it was first lexed. Full lex instead when the text contains `]:` or `\r`, when the raws don't sum to the text, when the text has `$$` and a kept token holds a `$$` line outside code and `latexBlock`, or when a kept token holds an inline tag that opened marked's `inLink` or `inRawBlock` (below). Then `trimPartialClosingFences` on the result; it mutates only the last token, which is never kept.
- **The margin and a list guard each make the other redundant.** Cutting before the last token passes every pair if a cut never follows a list (a growing `2` can become `2.` and join it); without the list guard, the second-to-last margin passes alone (0 of 256 pairs fail either way, 70 fail with neither). Kept: the margin, which needs no special case.
- **Inline state crosses blocks** (found by the E2E test, step 4, after the map): marked's inline lexer carries `state.inLink` and `state.inRawBlock` across the whole document. An inline `<a ` keeps later paragraphs' URLs from becoming links until `</a>`; `<pre>`, `<code>`, `<kbd>` and `<script>` mark later text raw. A slice lexed from a cut starts with both off. Each inline `html` token records both as they stand after it, and only these tags set them, so a kept token with one that recorded either on means a full lex (behind a text prefilter, so other answers skip the walk). Without the guard, 4 kinds of mismatch in 72 k checks; with it, none.
- **Verified:** zero mismatches on ~3 M checks of the final rule: 12 seeds × 6000 documents of every family but definitions (~22 % of checks on the incremental path, the rest guarded), the 16×16 pairs trimmed and untrimmed with pi-tui's parser and untrimmed with plain `new Marked()` (the mermaid transformer's, no `trimPartialClosingFences`), inline `<a `/`<pre>`/`<code>`/`<kbd>`/`<script>` in the words, and 24-byte streams of real text (`long_answer` at 20 and 60 KiB, this file, `local_plans/README.md`, `p6_lazy_modules.md`).
- **Link reference definitions are the one construct with no structural fix.** Without the `]:` guard, 13 kinds of mismatch in 84 k checks: inline links of earlier blocks, and block structure too, since a definition cannot interrupt a paragraph but can start a re-lexed slice. Every definition contains `]:` literally; so can prose (this file quotes it, and from that line on every frame is a full lex: 43 % of its characters re-lexed against 1.5 % for `long_answer`).
- **Lexing `long_answer` streamed in 24-byte frames** (`target/p8/probe/stream.mjs`): 99 % of frames incremental, 1.5 % of characters re-lexed, lex time 1.25 s → 0.11 s at 20 KiB and 12.0 s → 0.43 s at 60 KiB (quadratic → linear).
- **Deviation from the plan: `options.transform` is always set.** The assistant message passes the Markdown transformers (Pi's mermaid one, default mode `streaming`, plus extensions') to every text and thinking `Markdown`. Falling back on it would turn the pass off for every answer. The incremental path keys on the transformed text instead. The mermaid transformer's own full lex goes through the same incremental lex (owner, with O6); it still joins the token raws per frame, a linear copy.
- **Where `long_answer` goes on the default backend** (p7's `perf` profile, inclusive, all samples): `Markdown.render` 65 %, of which `renderToken` 18 %, `wrapTextWithAnsi` 15 %, highlight.js 9 %, marked `lexer` 5 %. The transformers 2.6 % (mermaid's full lex), the footer's `getContextUsage` 3.5 %. The per-block render cache is the larger lever; lexing is a twelfth of it.

**O6 (owner, 2026-10-20):** a pibake pass, not an upstream PR; fold in the mermaid transformer's full lex.

**Steps 3–4, the pass** (`src/passes/incremental_markdown.rs`, `src/bundle/markdown.mjs`):

- `Markdown.render`'s lex and its render → wrap → pad loop are matched by whole text and replaced, line count kept, by `pibake:markdown`'s `lexMarkdown` and a per-block function holding stock's statements in stock's order. The mermaid transformer's `markdownParser.lexer(markdown)` becomes `lexMarkdown(markdownParser, markdown)`. The pass also checks that Pi's theme global is still `Symbol.for("@earendil-works/pi-coding-agent:theme")`.
- **Lex cache:** the 8 most recent `{ parser, source, tokens }`, one per stream and parser; an extension replaces its entry. A component's own `WeakRef` token cache is gone (components are remade per update, so it hit only on re-renders, which the module cache covers).
- **Block cache:** a `WeakMap` from token object to `{ next type, context, lines }`, so it lives as long as the tokens. Kept tokens are the same objects across frames, so their padded lines are reused. Each frame lays the cached lines end to end.
- **Context key:** the plan's "theme" could not be an identity. `getMarkdownTheme()` returns new closures that read Pi's global theme, and `defaultTextStyle` is a new object per update. So the key is a string: width, `paddingX`, hyperlinks capability, the three render options, `codeBlockIndent`, whether `highlightCode` exists, each theme style and the default style's `color`/`bgColor` applied to `\0`, the style flags, and an id of Pi's theme object, which `setTheme` replaces whole. The id covers `highlightCode`'s syntax colours, which no probe sees.
- **Tests** (`tests/markdown.rs`, PIBAKE_E2E): the pass's real rewrite of the installed `markdown.js` against stock in one node process, through module hooks so both share pi-tui's other modules. A Hegel test (400 cases) streams documents from the map's constructs in 1–24-character or random deltas, trimmed (answer) or not (thinking), and changes width, Pi's theme, an extension theme's palette, any setting or the hyperlinks capability between deltas, or re-renders. Every frame's lines must equal stock's, and the plain-parser lex must equal a full one. Deterministic tests: the bench's answer at 24-byte deltas with a resize; each reach-back document a character at a time; a document that shows every setting, streamed while each changes alone. **Mutation check:** removing any guard, or any part of the context key, fails a test, except the raw-length check (defensive, never seen to fire) and the next-type comparison (unreachable under the cut rule; kept so the block cache does not rely on it).
- `verify` same on the live setup.

**Step 5, unrecorded** (load not idle, `bench --runs 3 --perf-stat --scenarios long_answer`, pass off → on, default backend): `long_answer_cpu_ms` 12 973 → 4 640 (−64 %), `long_answer_minstr` 75 802 → 14 026 (−81 %), peak RSS 265 → 255 MiB. `Markdown` alone (node, 24-character frames, no colours): 0.19 s at 20 KiB, 0.51 s at 40, 0.96 s at 60, against stock's 4.64 s and 19.5 s at 20 and 40. Not quite linear yet: each frame still walks every token and builds every line, which `render`'s contract returns.

**Step 6, recorded** (owner-confirmed idle machine, `target/p8/bench.sh`: `bench --runs 20 --perf-stat --record p8_markdown`, all scenarios, pass off → on, `sea_codecache`, live setup):

| Metric | Off | On | |
|---|---|---|---|
| `long_answer_cpu_ms` | 11 553 | 3 294 | −71 % (3.5×) |
| `long_answer_minstr` | 77 374 | 14 031 | −82 % |
| `long_answer_peak_rss_mib` | 266.9 | 252.9 | −14 MiB |
| `startup_minstr` / `startup_ms` | 2 439 / 402 | 2 440 / 402 | flat |
| `ready_ms` / `interactive_minstr` | 503 / 3 203 | 506 / 3 237 | flat (+1 % instructions, ranges overlap) |
| `big_write`, `tui_write`, `big_output` CPU | 1 104 / 2 922 / 967 | 1 109 / 2 926 / 959 | flat |
| `session_late_turn_cpu_ms` | 20.9 | 20.9 | flat |
| `session_end_rss_mib` | 307.4 | 312.6 | +5 MiB, ranges overlap |

Against stock's 14.5 s (p7's recording), `long_answer` is now 4.4× less CPU. Peak RSS rises 1–2 MiB at startup and interactive (the module's code). The session's +5 MiB is within its spread; the lex cache's 8 entries are a few answers' tokens, far less.

## Found

- **Two caches never meet.** A runtime-flag mismatch silently drops the code cache (V8's flag hash). Users who set `NODE_OPTIONS` with a V8 flag get the slow path with no message. p4's loader can see `cachedDataRejected`.
- **The snapshot backend's reason to exist is p4's to take.** If p4 lands with the numbers above, `sea_snapshot` is slower on ready and uses more memory, at a much higher maintenance cost (owner call O3).
