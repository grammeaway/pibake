# P6: Evaluate heavy, side-effect-free modules on first use

## Finding

**Regex and ICU set-up is 16 % of the trained build's main thread in RPC startup, and 22 % in interactive startup** (`progress.md` → Smaller costs). Most of it is module initialisation that RPC and print mode never use:

| Where | What | RPC | interactive |
|---|---|---|---|
| pi-tui `dist/utils.js` module init | `/v` regex literals with `\p{RGI_Emoji}`, `\p{Default_Ignorable_Code_Point}` … | 5.3 % | 4.7 % |
| the same module | an `Intl.Segmenter` at module level (ICU `Locale::createCanonical`) | 2.8 % | 2.5 % |
| marked module init (`getRegex`) | `new RegExp` with `\p{P}\p{S}` | 1.8 % | 1.6 % |
| typebox `format/idna/pattern.mjs`, `idn_email.mjs` | Unicode regex literals | 0.3 % | 0.3 % |
| pi-tui `graphemeWidth`, first `exec` | compiling the emoji regex | | 3.9 % |

- **Why this matters now:** the snapshot backend evaluates these at build time. A code cache, cold or trained (p4), can't: module evaluation runs at every launch.
- **What exists:** l10's lazy-module plugin in `build.mjs` replaces a module with an object that has a getter per export, and evaluates the real module on the first read. It handles the snapshot's unsafe builtins and undici.
- **What a lazy module can save:** the cost is only avoided if nothing reads an export during startup.
  - RPC and print mode never render a TUI or markdown.
  - Interactive startup renders the TUI, so pi-tui's utils run anyway. Marked does not run until a message is rendered.

## Change

Apply l10's lazy-module wrapper to every backend, for a fixed, measured list of modules (`FILES` in `src/passes/lazy_modules.rs`, the `lazy_modules` pass; as built, every backend but the snapshot, below). A module joins the list only if:

1. **Its top level has no side effects** other than creating its own values: no globals, no listeners, no I/O, no registration in another module. A reviewed allow-list, not a heuristic.
2. **No module reads its exports at its own top level on the startup path.** Otherwise the wrapper only adds an indirection. Step 1 checks this with a trace.
3. **Its removal from startup shows in `bench --perf-stat`.**

First candidates: `marked`, pi-tui `dist/utils.js`, typebox's `format/idna/*`.

This is a pibake build change (O5 also asks whether pi-tui wants lazy regexes upstream; that would make the pi-tui entry unnecessary).

## Steps

1. **Trace:** for each candidate, run RPC, interactive and print startup with the wrapper. Log the first read of each export, and its stack. A candidate read at a module's top level on the startup path is dropped, or the reader is noted for O5.
2. **Review side effects** of each remaining candidate's top level; record the verdict per module in this plan.
3. **Implement** the list for all backends. On the snapshot the effect should be zero: the snapshot already holds these modules evaluated. Check that it is.
4. **Measure:** `bench --runs 10 --perf-stat` on the default backend. Per candidate, keep it only with a `startup_minstr` drop beyond the spread.
5. **Tests:** `verify` same; an E2E test that renders a markdown reply in interactive mode (marked loads lazily and works); a unit test that a module outside the list is bundled as before.

## Outcome (2026-10-19)

**Trace (step 1).** A build that logged every first read of a stand-in's export, with the reading module, in RPC, print and interactive startup (live setup). Each listed module was first read at some other module's top level, so the readers joined the list until RPC and print mode loaded none of it:

| Module | Read at top level by | Verdict |
|---|---|---|
| marked `lib/marked.esm.js` | pi-tui `components/markdown.js` (`new Marked()`), coding-agent `components/mermaid.js` | listed, with both readers |
| pi-tui `dist/utils.js` | `autocomplete.js`, `word-navigation.js`, `components/editor.js`, `components/input.js`, `alt-screen-search.js`, `tui-alt-screen.js` | listed, with all six |
| pi-tui `components/editor.js` | coding-agent `components/custom-editor.js` (`class CustomEditor extends Editor`) | listed, with the reader |
| typebox `format/idna/pattern.mjs` | typebox `idna/puny.mjs`, every launch | dropped |
| typebox `format/idn_email.mjs` | typebox `format/_registry.mjs` (`Reset()` reads every checker) | dropped |

Interactive startup still loads all 11: InteractiveMode builds its mermaid cache in a member initializer, and the TUI needs pi-tui's utils. So the plan's "marked does not run until a message is rendered" is wrong for 1.3.0.

**Side effects (step 2)**, the 11 top levels: constants, regex literals, `Intl.Segmenter`s (`get*Segmenter()`), `Set`s and `Map`s, classes, and `new AnsiCodeTracker()` (`utils.js`). `markdown.js` and `mermaid.js` configure their own `Marked` instance (`setOptions`, `use`). No globals, `process`, timers, listeners or I/O; no `let`, `var` or static blocks at the top level.

**Implementation (step 3).**
- **Not on the snapshot (deviation).** A module evaluated after the resume misses `snapshot_safety`'s initialisers: `utils.js`'s deferred segmenters stayed `undefined`, and interactive startup crashed. The snapshot holds these modules evaluated anyway, so the pass is skipped there, with a note in the build log. Bundles on and off then differ only in the stage path, so there is nothing to bench.
- **Fixed exports.** A getter per export costs every call through it: ~25× a direct call in a micro-benchmark (esbuild's interop chains getters, and V8 does not inline them). Once loaded, an export no code reassigns becomes a plain value (found by oxc's semantic analysis); reassigned ones (marked's `defaults`) stay getters.
- Files are matched by path, not package name, as pi-tui imports its own files by relative path. A listed file the bundle never imports fails the build. `.mjs` files can't be listed: esbuild takes one for an ES module, and the stand-in is CommonJS.

**Measured (step 4)** per group, not per file: a group is deferred all or nothing. Group A is marked, `markdown.js` and `mermaid.js`; group B is `utils.js` and its seven readers, which needs A's readers off the startup path too. `progress.md` → p6 has the table: startup_minstr −15.4 % on `sea_codecache` (A alone −3.7 %), −17.2 % on `sea_trained`. Interactive ready +10 ms on `sea_codecache`, which loads these modules anyway (owner: keep all 11, default on).

## Completion

- [x] Every listed module has its trace, its side-effect review and its measured drop recorded here (the drop per group).
- [x] `verify` same; markdown and TUI rendering unchanged by test (`tests/lazy_modules.rs`).
- [x] `cargo fmt --check`, clippy and every test green, E2E included.

## Non-goals

- Lazy evaluation as a general bundle strategy. Module order is observable, so only reviewed modules get it.
- The interactive-only costs (`graphemeWidth`'s regex, the TUI's segmenter). The TUI needs them at startup.
