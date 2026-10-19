# Performance plans: progress

Live log for `README.md`'s plans: status, decisions, and the probe round's findings.

## Status

| Plan | Status | Notes |
|---|---|---|
| p1 pty bench | done (`6cd7677`; recording later) | `first_paint_ms`, `ready_ms`, `interactive_cpu_ms`, `interactive_peak_rss_mib`; `--big-write-kib 0` skips the big write. Step 4's breakdown is under Probe round. Recorded ready: stock **2663 ms**, warm code cache **494**, snapshot **374**; interactive peak RSS 377 / 192 / 222 MiB (`bench_results/2026-10-17_p1_interactive.json`). A first attempt was discarded: the CPU was stuck at 800 MHz after a suspend |
| p2 perf probes | done (`6cd7677`) | `build --profiling`, `bench --perf-stat` (`*_minstr`), README Profiling section; breakdown under Probe round |
| p3 ASCII bundle | done | `ascii_bundle` pass, default on; `verify` same on the live setup for both backends. Recorded: peak RSS −13 MiB (code cache), −52 MiB (snapshot); code-cache startup instructions +1.4 % (deviation, below) (`bench_results/2026-10-17_p3_ascii.json`) |
| p4 trained code cache | planned | the probe round's headline |
| p5 lazy HTTP dispatcher | planned | |
| p6 lazy TUI regexes | planned | |
| p7 session bench | planned | |
| p8 incremental Markdown | planned | waits for O6 and p7 |

## Decisions

| Plan | Decision | Taken |
|---|---|---|
| p1 | `Screen` parses escape sequences byte by byte, so a sequence split across reads needs no carried tail (the plan said "a short tail") | 2026-10-17, agent |
| p1 | Readiness checks all text drawn up to ready for the model id, not only the final frame: Pi redraws changed lines only, so the last frame often lacks the footer | 2026-10-17, agent |
| p2 | Profiling flags per backend: code cache gets `--perf-basic-prof --interpreted-frames-native-stack`; the snapshot only `--perf-basic-prof`, since a snapshot built with both segfaults at launch (Node 26.11.1) | 2026-10-17, agent |
| p3 | `ascii_bundle` runs in `Bundler::run` on the final bundle, not in `passes::run`, so it is listed in `[build] passes` but skipped there. Its report's file is bundle-relative (`main.mjs`, `main.cjs`) | 2026-10-17, agent |
| p3 | A non-ASCII character after `\` in a regex (an identity escape, non-`u` only) is written as one escape in place of both, since `\` before the escape would make it a literal `\u` | 2026-10-17, agent |
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

## Found

- **Two caches never meet.** A runtime-flag mismatch silently drops the code cache (V8's flag hash). Users who set `NODE_OPTIONS` with a V8 flag get the slow path with no message. p4's loader can see `cachedDataRejected`.
- **The snapshot backend's reason to exist is p4's to take.** If p4 lands with the numbers above, `sea_snapshot` is slower on ready and uses more memory, at a much higher maintenance cost (owner call O3).
