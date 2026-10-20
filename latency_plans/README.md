# Latency plans

**Goal:** close the gaps between what a user waits for and its napkin-math floor. The floors come from `~/code/napkin-math` (README numbers) and local reference runs; the measurements from pibake's probes (`bench --perf-stat`, `build --profiling` with `perf record`, `--cpu-prof`) and the taps in `target/n1/`. The probe round is in [`progress.md`](progress.md).

**Baseline:** pibake `d52bb70` (v0.2.0), Pi 1.3.0, Node 26.12.0, the live setup, `sea_codecache` with every default pass (`pi-p8`). Instruction counts match p8's recording (startup 2439 M against 2440, big write 5516 against 5515), so the setup has not drifted.

The shared rules of `../local_plans/README.md` apply, and `../performance_plans/README.md`'s verdicts and candidates stand.

## Where it stands

What a user waits for, against its floor:

| Wait | Measured | Floor | Gap | Verdict |
|---|---|---|---|---|
| A new provider connection, per prompt after ≥ 4 s idle | 55–65 ms (DNS, TCP, TLS to `api.anthropic.com`, ~20 ms RTT) | 0: a pooled connection | all of it | **n1** |
| First prompt of a session: prompt to the provider's first byte | 64–91 ms (RPC, local provider); +106 ms per print turn (p4) | ~7 ms: a warm turn | ~10× | **n2** |
| Warm turn: prompt to first token | 6–8 ms, with up to 720 KiB of context | ~7 ms: 720 KiB of JSON at napkin's 100 MiB/s | at the floor | none |
| Tool turnaround: `read` done to the next first token | 5–8 ms | same as a warm turn | at the floor | none |
| A streaming frame (`long_answer`, 20 KiB) | ~3 ms of CPU | ~0.2 ms: the viewport and the last block | ~15× | Below the 16 ms frame budget. 31 % of it is the live setup's `statusbar.js` (Outside pibake, below) |
| A print-mode delta (`big_write`) | ~10 µs per 20-byte delta; 12 k instructions per byte, linear (1, 200, 400 KiB: 3035, 5516, 7905 M) | 2–5 µs | 2–5× | none: a real stream's deltas arrive milliseconds apart |
| Exit: Ctrl+D to the shell | 90–116 ms | ~2 ms | ~50× | 50 ms of it is a deliberate wait (Candidates) |
| RPC startup, interactive ready | 402, 505 ms (p8 recording) | ~40 ms: `node -e 0` is 105 M instructions against 2439 M | ~10× | Structural: JS evaluation and lazy compilation, which p3–p6 and the backends address. Interactive's extra: Candidates |
| 8 MiB of bash output | ~0.3 s of CPU | ~50 ms | ~6× | none: real outputs are kilobytes |

- **The two gaps that matter are both at the start of a model call.** A human prompt usually pays n1 (~60 ms); the first prompt of a session pays n1 and n2 (~120–150 ms). Even a 401 takes 200–400 ms to come back from the provider here, and a model's first token takes longer. So these gaps are roughly 5–15 % of the wait for a first token, not an order of magnitude.
- **Everything per token or per turn after the first is at or near its floor.** l11, p7 and p8 took the quadratic costs out. What is left is a constant factor of 2–5 against napkin numbers.

## Plans

| Order | Plan | What | Gate |
|---|---|---|---|
| 1 | [`n1_idle_keepalive.md`](n1_idle_keepalive.md) | `keepAliveTimeout` on Pi's dispatcher, below the provider's own idle timeout: −55–65 ms per prompt after ≥ 4 s idle | O7; live check by the owner |
| 2 | [`n2_first_turn_prewarm.md`](n2_first_turn_prewarm.md) | Evaluate the provider SDK and create the dispatcher in idle time after ready: about −40–60 ms on a session's first prompt (interactive, RPC) | O8; bench: idle machine |

**Why this order:** n1 is a one-option change with the larger total (every idle prompt). n2 is a larger change for one prompt per session. They don't depend on each other. If both land, the first-connection handshake is the only cold cost left on a first prompt (Candidates: preconnect).

## Open owner calls

| # | Call | Needed by |
|---|---|---|
| O7 | n1: a pass, default on or opt-in, or an upstream PR to Pi. **Decided:** a pass, default on (`progress.md`) | n1 step 3 |
| O8 | n2: a pass, default on or opt-in, or an upstream PR to Pi | n2 step 3 |

## Outside pibake: the live setup's status bar

The live setup's own `extensions/statusbar.js` is 31 % of `long_answer`'s CPU after startup (`--cpu-prof`). Its footer calls `ctx.getContextUsage()` and walks every session entry on every frame. Its header calls `createRequire` and `require` for the version, and `os.userInfo()`, on every frame. On a session's first answer, `getContextUsage` estimates every message, including `JSON.stringify` of the system message's tool declarations (`estimateTokens`), so `estimateTokens` alone is 24 % of the CPU. Pi's own footer caches the same values (`getSessionStats`, keyed by session id, leaf id, entry count and model). **Done (2026-10-20, owner's go):** `statusbar.js` now caches the usage totals and `getContextUsage` on Pi's footer key (session manager, session id, leaf id, entry count, model) and reads the version and user name once. `long_answer`, 3 runs: 14125 → 10584 M instructions (−25 %). The original is in `target/n1/statusbar.js.orig`.

## Candidates

Each needs a measurement before it becomes a plan.

- **Preconnect** to the current model's origin once it is known. That hides the first connection's 55–65 ms: behind typing in interactive mode, behind startup in print mode. undici has no public preconnect, so it means a cheap request to the provider or undici internals. An owner call either way; after n1.
- **The exit wait.** `shutdown()` → `terminal.drainInput(1000, 50)` waits for 50 ms without input, so key-release events of the Kitty keyboard protocol don't leak into the shell. Without a negotiated protocol nothing can leak, and the 50 ms is pure wait (as in the bench's pty). Ctrl+D to exit: 90–116 ms, 20–35 ms of it JS (re-rendering the transcript for the normal screen, `dispose`). Small; an upstream matter.
- **Interactive-only init** (`--cpu-prof`, `init` onward, at the probe's clock): `graphemeWidth`'s first call 16 ms (ICU segmenter set-up, for lines with any non-ASCII character such as box drawing), `ensureTool`'s `spawnSync` 8 ms (`../performance_plans/README.md` Candidates), highlight.js registration 5 ms, `parseChangelog` 3 ms. Together ~30 ms of ready's ~100 ms over RPC.
- **`createContext` per extension event**: 9 % of a print-mode stream's CPU after startup, a new context object per delta. Below the floor that matters (2–5×, row above).
