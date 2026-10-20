# Performance plans

**Goal:** make the built executable start faster and use less memory where users feel it: RPC startup, interactive ready, the first turn, and (p7, p8) CPU and memory over a session. p1 and p2 built the instruments; their probe round (`progress.md`) says where the time and memory go. p3 to p6 act on it. p7 does the same for sessions, and p8 is its one fix in this pass. This plan list is final for the pass; what p7 ranks goes to the next round.

**Baseline:** pibake at `b44ccc2` (l11), Pi 1.3.0, Node 26.11.1. Recorded numbers in `../bench_results/`; history in `../local_plans/progress.md`, this round's in [`progress.md`](progress.md).

The shared rules of `../local_plans/README.md` apply. A perf-sensitive plan also measures with `bench --perf-stat`: `*_minstr` is far less noisy than time on this laptop.

## Where it stands

Indicative, from the probe round (5 runs, same lock):

| | stock | `sea_codecache` (default) | `sea_snapshot` | trained cache + ASCII (p3 + p4 prototype) |
|---|---|---|---|---|
| RPC startup | ~2600 ms | 450 ms | 325 ms | 325 ms |
| interactive ready | ~2800 ms | 499 ms | 386 ms | 368 ms |
| startup peak RSS | 366 MiB | 181 MiB | 213 MiB | 170 MiB |

On the default backend:

- **40 % of the main thread at startup is compiling functions the code cache never saw.** That is p4.
- **41 MiB of memory is the bundle source held as UTF-16,** because ~1,400 characters are not ASCII. That is p3.
- After both, the largest costs left are:
  - Pi's eager HTTP dispatcher: 8 % of RPC startup (p5);
  - regex and ICU set-up in modules RPC mode never uses: ~10 % (p6);
  - `createAgentSessionRuntime`: about 130 ms, of which extensions take 49.

## Plans

| Order | Plan | What | Gate |
|---|---|---|---|
| 1 | [`p1_pty_bench.md`](p1_pty_bench.md) | Interactive startup through a pty: first paint, ready | bench: idle machine |
| 2 | [`p2_perf_probes.md`](p2_perf_probes.md) | `build --profiling`, `bench --perf-stat`, the probe round | none |
| 3 | [`p3_ascii_bundle.md`](p3_ascii_bundle.md) | Escape the bundle's last non-ASCII characters (comments, regex literals): −12 MiB on the code cache, −20 on the snapshot | bench: idle machine |
| 4 | [`p4_trained_code_cache.md`](p4_trained_code_cache.md) | A `sea_trained` backend: the code cache taken after three harness runs at build time. About −28 % RPC startup and −26 % ready | O4 before step 2; bench: idle machine; then O3 |
| 5 | [`p5_lazy_http_dispatcher.md`](p5_lazy_http_dispatcher.md) | Create Pi's undici dispatcher on first use, not at `main()`'s start: about −25 ms startup and ready | O5 |
| 6 | [`p6_lazy_modules.md`](p6_lazy_modules.md) | Evaluate marked, pi-tui's utils and the modules reading them at load on first use, after a trace and a side-effect review | O5 (pi-tui part) |
| 7 | [`p7_session_bench.md`](p7_session_bench.md) | Session scenarios in `bench`: a long answer, a write and 8 MiB of bash output in the TUI, 40 RPC turns. Profile and rank each | bench: idle machine |
| 8 | [`p8_incremental_markdown.md`](p8_incremental_markdown.md) | Lex and render only the last two blocks of a streaming answer: probe 36.7 s of CPU for 60 k characters, quadratic | O6; bench: idle machine |

**Why this order:**

- p3 comes first: it is small, it helps both existing backends, and p4's memory numbers assume it.
- p4 is the largest lever; p5 and p6 are measured on top of the default `sea_codecache` and on `sea_trained` (O3, O4: `progress.md` → p4).
- p5 and p6 don't depend on each other.
- p7 and p8 are about a session, not startup, and are independent of p3 to p6. p8 needs p7's `long_answer` number.

p1 and p2 are implemented but not committed; their recorded bench (p1 step 5) can run with p3's.

## Open owner calls

| # | Call | Needed by |
|---|---|---|
| O6 | p8: a pibake pass, or an upstream PR to pi-tui | p8 step 2 |

## Verdicts: Cloudflare memory posts (2026-10-17)

Sources: [DNS cache](https://blog.cloudflare.com/dns-cache-memory-optimization-1111/) (the 1.1.1.1 cache post) and [ketama](https://blog.cloudflare.com/saving-100-tb-of-ram-with-math/) (the consistent-hashing post). Both tricks shrink Rust structures that exist billions of times. pibake has no such structures, and the executable it builds has no Rust in it. `pibake bundle` with every pass on the live setup peaks at about 11 MiB RSS (`VmHWM`) in 2.4 s.

| # | Technique | Verdict |
|---|---|---|
| 1 | `Box<[T]>` and `Box<str>` instead of `Vec` and `String`, one list plus `u16` offsets, bitflags (DNS cache) | **Dropped.** pibake's structures hold dozens of entries, once per build |
| 2 | Box the large enum variants (DNS cache) | **Dropped.** Same reason |
| 3 | Store the compact encoded form instead of the parsed one (DNS cache) | **Adopted, translated to V8:** p3, the string encoding of the bundle source |
| 4 | Infer data that is the same for every entry instead of storing it, e.g. the record owner (DNS cache) | **No target.** V8 already interns string literals and shares hidden classes, so the model catalog's repeated keys and values are stored once |
| 5 | Measure bytes per entry with a counting allocator, then check whole-process RSS (DNS cache) | **Adopted as method:** the probe round's heap and mapping breakdown (`target/probe/memprobe.cjs`) |
| 6 | Pack a struct into `[u8; 6]` to remove alignment padding (ketama) | **Dropped.** No large arrays of small structs, in pibake or in Pi's hot paths |
| 7 | Cut a default sized by convention, after checking the error with math (ketama) | **Dropped.** Nothing in the binary is generated N times by a conventional constant. The closest cases, highlight.js languages (l8) and provider APIs (l9), are already cut from what the setup uses |

## Verdicts: native code, bundled tools, Pi-Bolt (2026-10-17)

The question: replace the hot parts of Pi with Rust, or bake faster tools into the executable, as Pi-Bolt is said to be orders of magnitude faster. Sources: [Pi-Bolt](https://github.com/opensec-git/Pi-Bolt) 0.9.4 (`docs/ARCHITECTURE.md`, `docs/BENCHMARKS.md`, `docs/releases/`), a diff of its `packages/` against Pi v1.3.0, and probes on this machine (`progress.md` → Session probes).

**How Pi-Bolt does it:**

- **Startup and memory: a different engine.** It is Pi's own TypeScript (a fork, v1.3.0) on a patched Bun with an experimental JavaScriptCore AOT compiler ([oven-sh/WebKit#743](https://github.com/oven-sh/WebKit/pull/743), plus its own x86-64 port). Every function is machine code, and the evaluated heap is mapped from the file: ready to type in 38 ms, against 116 for stock Pi on Bun and 296 on Node (no extensions).
- **The orders of magnitude: fixes to Pi's own JavaScript, not the engine.** Stock Pi on Node does a long answer in 38.7 s of CPU, a 200 KB write in 32.5 s. Pi-Bolt does them in 2.6 s and 0.2 s. On stock Bun's JIT, the same Pi changes land within 3 % of the AOT build (2.98 against 3.05 s, ARM64).

| # | Idea | Verdict |
|---|---|---|
| 1 | Rust in place of hot JS components (N-API addon or WASM in the executable) | **Dropped for now.** There are three reasons. **Startup** is V8 parsing, compiling, deserialising and evaluating the JS that has to load anyway (probe round); a native leaf does not remove that, but p4 and p6 do. **A session's** hot paths are quadratic algorithms in Pi's JS, such as l11's tool JSON and p8's Markdown. Fixing the algorithm gives 10× or more, and a faster language gives a constant factor on a quadratic; Pi-Bolt's Bun-versus-AOT numbers say the same. **The cost** is a Rust build per target, a `.node` file extracted and `process.dlopen`ed at launch, UTF-16 to UTF-8 copies on calls that run per line per frame, and parity with JS semantics such as width tables and highlight.js output. Revisit when a profile after p8 names one CPU-bound leaf function with ≥ 5 % of a scenario |
| 2 | Bake ripgrep and fd into the executable | **Not a performance item.** Pi already uses `rg` (grep tool, bash `PATH`) and `fd` (find tool, `@` completion), from `~/.pi/agent/bin` or `PATH`, downloaded on first interactive start (`utils/tools-manager.js`). Baking pinned copies makes a build hermetic and offline-safe, which is for the shareable-setups round. The startup part is the lookup (Candidates) |
| 3 | Point the bash tool at faster tools (`grep` → `rg` and the like) | **Dropped.** Same name, different semantics: flags, output format, `.gitignore` filtering. It changes results and breaks `verify` parity. Which command the model picks is a prompt matter, and command run time is small next to model latency |
| 4 | Pi-Bolt as a backend (its runtime and compile kit) | **Dropped.** It compiles Pi-Bolt's forked Pi, not released Pi (pibake's "Pi itself is not forked"), on one organisation's patched Bun and an unmerged WebKit branch. Watch it |
| 5 | Pi-Bolt's AOT on our engine | **Not available.** V8 has no ahead-of-time machine code. The nearest are p4's trained code cache (bytecode) and the snapshot |
| 6 | An opinionated build flag for invasive changes | **Not needed.** `[build] passes` is already the switch. Passes that draw and return what stock does (p8) are default-on. A change that alters behaviour, such as baked tool versions, would be its own opt-in setting, with its own verdict |
| 7 | Pi-Bolt's Pi-level fixes | **Adopted where measured.** Tool-call JSON is l11. Long answers are p7 and p8. The rest are Candidates below |

## Candidates

Each needs a measurement before it becomes a plan.

- **`minifyWhitespace`** on the bundle. It saved another 11 MiB on top of p3 on the code cache, with instructions unchanged. Open questions: stack traces and `Function.prototype.toString` users. Measure on top of p4.
- **The first extension import costs 22 ms** on both built backends (`init.ts`, 59 lines; the others ~1 ms). It is a one-off cost of the first baked load, not yet attributed.
- **`createAgentSessionRuntime` beyond extensions** (~80 ms): resource loading (`loadSkills`), the model catalog (`getModels`, `updateModelSnapshot`), typebox schema compile and clone. Profile on top of p4.
- **Provider auth checks at startup** (`updateAvailableProviderCount`, `ModelRuntime.create`): l9 option A, declined for its `/login` and `/model` effects.
- **Model catalog as a JSON string, parsed on first access** (DNS cache #3). Part of the item above; no behaviour change, unlike l9's declined option A.
- **Node's built-in undici loaded at startup** by Pi's `originalGlobalFetch = globalThis.fetch` at module load (`progress.md` → p5). Unmeasured.
- **`node::crypto::LoadCACertificates` at startup** (~1.5 % of the main thread).
- **Managed-tool lookup at interactive startup:** `ensureTool("fd" / "rg")` runs before the key handlers and `renderNow()`. For a tool that is not in `~/.pi/agent/bin`, it calls `spawnSync(<tool>, ["--version"])` (`utils/tools-manager.js` `commandExists`). Measured 3–6 ms per tool on `pi-baked`, and up to ~10 ms each from a large process, plus `fdfind` when `fd` is missing. A `PATH` scan with `accessSync(X_OK)` gives the same answer for a missing binary. It is the only subprocess at startup on the live setup. A pass or an upstream PR, as O5.
- **Session costs, ranked by p7** (`progress.md` → p7; costs on the default backend, live setup). Input for the next round:

  | # | Scenario | Cost | Where (Pi 1.3.0) | Likely fix |
  |---|---|---|---|---|
  | 1 | `long_answer` | ~80 % of the CPU after startup, quadratic; 11.5 s for 20 KiB | pi-tui `Markdown.render`: marked `lex`, highlight.js and `wrapTextWithAnsi` over the whole answer per frame | **Done, p8:** 3.3 s, −71 % |
  | 2 | `tui_write` | 44 % of ~3 s for 200 KiB, quadratic | `formatWriteCall` (write tool `renderCall`) formats the whole content so far on every delta | Pi-Bolt's "redraw only when the arguments change" and "highlight the lines a collapsed call shows"; l11's new object per delta defeats an identity guard, so key on the content's length |
  | 3 | `long_session` memory | +130 MiB over 40 turns (~300 MiB at the end against ~170 after startup; stock ~510) | 34 MiB of live large objects and 16 MiB of array buffers for 720 KiB of content; the young generation grows to its cap (+39 MiB reserved) | A heap snapshot after 40 turns, to find what keeps whole-context strings; then possibly `--max-semi-space-size` (l1 verdict 18) |
  | 4 | `long_answer`, beyond p8 | ~13 % (stock profile) and 2–3 % | the mermaid transformer lexes the whole answer again per render; the footer's `getContextUsage` → `estimateTokens` per frame | The mermaid lex: **done in p8** (owner, O6). The footer: our own, cache by text |
  | 5 | `long_session` CPU | 25–35 ms per turn, flat over 40 turns | per request: the OpenAI SDK's body `JSON.stringify` (11 %), the tool declaration diff (`declareToolChanges`, 11 %), pi-btw's `structuredClone` (4 %) | None at this size; revisit with longer sessions |
  | 6 | `big_output` | less than startup for 8 MiB; the output path 2–5 % | | **Dropped:** Pi-Bolt's bash-output fixes target 1.1.0 and 1.2.0 |

  Not covered by a scenario: the edit tool's jsdiff over the whole file, up to three times per edit.
- **The TUI's frame diff** (Pi-Bolt's "stable render"): **low** on 1.3.0. Its caches already make re-rendering 400 messages (17,600 lines) 0.19 ms per frame (`progress.md` → Session probes), and the screen post-processing is viewport-only. The fix changes the component contract across about 12 classes.
- **Small Pi-level fixes from Pi-Bolt, unmeasured here:**
  - undo and paste without copying the whole editor per keystroke (pi-tui `undo-stack`, `editor`, `stdin-buffer`);
  - one-pass `truncate*` and `read`;
  - no `structuredClone` of the context when no extension handles `context`. That one does not apply to the live setup, since `pi-btw` handles it.
- **A Bun backend** (`bun build --compile --bytecode` of Pi's `dist/bun` and the baked extensions). Pi-Bolt measured stock Pi on Bun against Node: ready 116 against 296 ms, CPU per `-p` 283 against 550 ms, own memory in tmux 84 against 185 MB. It needs D4 revisited, a second runtime to support, and extension compatibility checks. Probe it after p4, against `sea_trained`.

Settled by the probe round:

- **The snapshot's big-write RSS** is now +30 MiB over the code cache, not +150 (l11's parser fixed most of it).
- **The eager-compile (PIFE) pass** is superseded by p4, which compiles what startup runs, not every module initialiser.
- **The snapshot's uncompiled code after the resume** is p4's problem to solve on the default path. The snapshot plus a trained cache was tried and rejected by `mksnapshot`.
