# P1: Interactive startup through a pseudo-terminal

## Finding

`pibake bench` measures startup as RPC readiness (`get_state` answered). What users feel is the interactive TUI: when the first frame shows, and when Pi has finished starting. Neither is measured (l2 step 6, l8 and l11 Found notes), so the effects that land after the first frame (`trim_highlight`, extension `session_start` handlers) do not show in any number we have.

Pi 1.3.0's interactive startup (`dist/main.js`, `dist/modes/interactive/interactive-mode.js` `init()`):

1. Module evaluation, then `createAgentSessionRuntime`, which loads extensions. This is before anything is drawn, so most of stock's ~2.6 s, and most of pibake's win, comes before the first frame.
2. `ui.start()`: editor and footer. **First paint.**
3. `themeController.waitForTerminalColors()`: Pi sends the colour query ending in DA1 (`\x1b[c`) and waits for the DA1 reply, or 100 ms (`TERMINAL_QUERY_TIMEOUT_MS`). Then the header renders.
4. `ensureTool("fd" / "rg")`, then the key handlers are bound.
5. `rebindCurrentSession()`: the extensions' `session_start` handlers.
6. `updateAvailableProviderCount()`.
7. `ui.renderNow()`: **startup complete.**
8. `loadAllHighlightLanguages()`, then one more render.

Pi has the hooks a harness needs:

- `PI_STARTUP_BENCHMARK=1` runs `interactiveMode.init()` to its end, waits 150 ms for terminal replies, stops the TUI and exits (`main.js`, interactive mode only).
- `PI_TIMING=1` prints per-phase times to stderr (`core/timings.js`). They are relative to the first mark in `main`, so they leave out module evaluation.
- Both renderers wrap each frame in synchronized output (`\x1b[?2026h` … `\x1b[?2026l`; `tui-alt-screen.js`, `tui-main-screen.js`), so frame ends can be found in the byte stream.

## Change

`pibake bench` gains an interactive startup per command and round, run in a pty under the existing `Harness`:

- **Metrics** (spawn to frame end, unless stated otherwise):
  - `first_paint_ms`: end of the first frame;
  - `ready_ms`: end of the `renderNow()` frame (step 7);
  - `interactive_cpu_ms`, `interactive_peak_rss_mib`: the whole process, from `Supervised::wait`.
- **Pty:** `libc::openpty` with a fixed window size (120×40). The child gets `setsid` and `TIOCSCTTY` in `pre_exec`. stdin and stdout go to the pty; stderr goes to the harness log, so `PI_TIMING` output stays out of the frame stream. No new dependency.
- **Terminal emulation, minimal:** a reader thread drains the master until EOF, timestamps each frame end, and answers DA1 (`\x1b[c`) with `\x1b[?62c`, as a real terminal does within a millisecond. Without the reply, every run would pay the 100 ms timeout. Markers can straddle reads, so the scanner keeps a short tail. Nothing else is answered: a colour-less reply is identical for every command.
- **Command:** `harness.command(pi, ...)` with `PI_STARTUP_BENCHMARK=1`, `--no-session --provider pibake_fake --model hello` (as the RPC startup), and `TERM=xterm-256color`.
- **Startup-only runs:** `--big-write-kib 0` skips the big write. Today one stock big write costs ~48 s per round, which makes startup-focused iteration slow (l7 Found).

## Steps

1. **Prototype the scanner** on one stock run and one code-cache run under the harness, with `PI_TIMING=1`, and keep the raw pty bytes. Check:
   - Pi chooses interactive mode with a pty on stdin and stdout;
   - there is no onboarding, changelog or trust dialog in the harness home. If there is one, the harness settles it the same way for every command;
   - which frame is `renderNow()`'s. The highlight re-render (step 8) may land inside the 150 ms wait. If the two cannot be told apart reliably, `ready_ms` is the last frame end before teardown, and the plan logs that it may include step 8;
   - the teardown (alt-screen exit) is not counted as a frame;
   - the DA1 reply ends the colour wait (`PI_TIMING` `interactiveMode.init` drops by ~100 ms compared with no reply);
   - both backends, and both `tuiMode` values (`fullscreen`, the default, and `regular`).
2. **Implement** in `src/bench.rs` (with the pty spawn in `src/process.rs` beside `Supervised`), and the `--big-write-kib 0` skip.
3. **Readiness check:** the run fails if no frame came, if Pi exits non-zero, or if the final frame does not show the fake model id (`hello`). That way a run that broke early cannot pass as a fast one.
4. **Breakdown:** one `PI_TIMING=1` run per backend (stock, `sea_codecache`, `sea_snapshot`), logged in this plan's progress as where interactive startup goes. Indicative, not recorded. This is the input for the next performance plans.
5. **Gate:** ask the owner for an idle machine. Record `pibake bench --runs 20 --big-write-kib 0 --record p1_interactive` with stock, warm-cache code cache (as l10) and snapshot, all built from one lock.

## Completion

- [x] The new metrics appear in `bench` output and in recorded JSON for every command.
- [x] A unit test of the frame scanner on captured bytes, with markers split across reads and a DA1 query mid-stream. A Hegel property test also checks that splitting the stream at arbitrary points changes nothing.
- [x] `PIBAKE_E2E=1` test: the bench runs interactive startup on the fixture setup with stock Pi and a built executable, and the readiness check holds. **Deviation:** it asserts `first_paint_ms <= ready_ms`, not `<`. Pi's first render races the instant DA1 reply; when the render loses, the first frame is `renderNow`'s own (about half the runs; `progress.md`).
- [x] A deliberate break (no DA1 reply) shows as ~90 ms more `ready_ms`, and nothing else changes (by hand, reverted).
- [ ] Recorded bench on an idle machine in `bench_results/`.
- [x] `cargo fmt --check`, clippy and every test green, E2E included.

## Non-goals

- A terminal emulator: no screen model, no colour replies, no keyboard-protocol replies beyond DA1.
- Typing latency or TUI redraw under streaming (l11 Found). The scanner makes that possible later, but it is a separate measurement.
- Any optimisation. This plan only produces the number.
