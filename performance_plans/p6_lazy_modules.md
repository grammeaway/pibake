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

Apply l10's lazy-module wrapper to every backend, for a fixed, measured list of modules (`LAZY_MODULES` in `build.mjs`). A module joins the list only if:

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

## Completion

- [ ] Every listed module has its trace, its side-effect review and its measured drop recorded here.
- [ ] `verify` same; markdown and TUI rendering unchanged by test.
- [ ] `cargo fmt --check`, clippy and every test green, E2E included.

## Non-goals

- Lazy evaluation as a general bundle strategy. Module order is observable, so only reviewed modules get it.
- The interactive-only costs (`graphemeWidth`'s regex, the TUI's segmenter). The TUI needs them at startup.
