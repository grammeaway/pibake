# Local release builds: progress

Live log for `README.md`'s plans. One row per plan; details under Deviations and Found.

## Status

| Plan | Status | Base | Notes |
|---|---|---|---|
| l1 scaffold, inspect | done | db3648d | Pi 1.3.0 (plan: 1.2.0); Hegel for property tests; Pi-resolver oracle test added |
| l2 bench, verify | done | b50e3d9 | stock baseline: `bench_results/2026-10-16_stock_baseline.json` (20 runs) |
| l3 lock | planned | | |
| l4 stage | planned | | |
| l5 bundle | planned | | |
| l6 SEA backend | planned | | bench needs an idle machine |
| l7 staleness | planned | | |
| l8 passes, trim highlight | planned | | |
| l9 fold providers | planned | | |
| l10 snapshot backend | planned | | O2 after step 1 |
| l11 streaming JSON | planned | | O1 |

## Decisions

Decisions taken during passes, with who took them (owner or agent) and when.

| Plan | Decision | Taken |
|---|---|---|
| l1 | Property tests use Hegel (`hegeltest` 0.49, `static-engine` feature, so no runtime-loaded engine library) instead of `proptest`. Example database `.hegel/` is gitignored | 2026-10-16, agent, on the owner's stated preference |
| l1 | Plain fixtures (`tests/fixtures/agent_dir/`) plus `serde_json` asserts, not `insta`: the asserted JSON is small and stable | 2026-10-16, agent |
| l1 | `rust-toolchain.toml` pins `stable`: `~/code` has a nightly directory override, which the repo would otherwise inherit | 2026-10-16, agent |
| l1 | `pibake.toml`: a relative `agent_dir` resolves from the directory holding the file (so repo mode's `"."` means the repo). Global `--config <path>`; default `./pibake.toml` if present, else built-in defaults | 2026-10-16, agent |
| l1 | A source that Pi would hand to `hosted-git-info` (e.g. `git:owner/repo`, `git:github:owner/repo`) is an error, not a guess. Pi itself falls back to treating unparseable git sources as local paths; pibake refuses rather than silently resolve something else | 2026-10-16, agent |
| l1 | Inventory has three top-level classes: `config` (Pi's documented config names, `agents/`, and any top-level entry a local package or a `settings.json` resource path points at), `install_roots` (`npm/`, `git/`, read only for package manifests), and `state` (names only, never opened) | 2026-10-16, agent |
| l2 | Each run gets a throwaway `$HOME`: agent dir at `$HOME/.pi/agent` (also `PI_CODING_AGENT_DIR`), `~/.agents` linked from the real home, cwd beside it. The live `extensions/usage.ts` reads `homedir()/.pi/agent/auth.json` and calls Anthropic's OAuth usage API at startup; under the real `HOME` the harness would read live credentials and go online. Also settles l1's Found note on controlling `HOME` | 2026-10-16, agent |
| l2 | Harness staging: config files copied, config directories and install roots symlinked (read-only use), `models.json` written as the user's file plus `providers.pibake_fake`. State is left out, including `bin/` (Pi fetches fd/rg only when `find`/`grep` run, and `PI_OFFLINE` blocks it) and `models-store.json` (catalog cache: both sides skip parsing it alike) | 2026-10-16, agent |
| l2 | Pi runs with a cleared environment: `PATH`, `LANG`, `LC_ALL`, `LC_CTYPE`, `TZ`, `TMPDIR` passed through, plus `HOME`, `PI_CODING_AGENT_DIR`, `PI_OFFLINE=1`, `PI_SKIP_VERSION_CHECK=1`, `PI_TELEMETRY=0`. This saga runs inside Pi (`PI_SESSION_*` would leak), and provider keys must not reach the run | 2026-10-16, agent |
| l2 | Fake provider: std `TcpListener`, a thread per connection, `Connection: close`, `Content-Length` bodies only (chunked is refused loudly). No HTTP dependency. New dependencies: `libc` (`wait4` usage, `killpg`); `tempfile` moved from dev to normal | 2026-10-16, agent |
| l2 | The script is picked by model ID (`hello`, `big_write`) on one server. Only a request that offers `write` and does not follow a tool result gets the big write; extension side calls (the observational-memory observer uses the current model) get a short text | 2026-10-16, agent |
| l2 | `verify` takes the snapshot from the first model request of the run (asserted to open with the system message) and adds `request_options` (the request body minus messages and tools) to the plan's three parts: it is model-visible and free | 2026-10-16, agent |
| l2 | `bench`: one discarded warm-up round, the first command rotates per round, startup also records the startup process's CPU and peak RSS (free from `wait4`). Spread is the interquartile range; raw samples are kept in the JSON | 2026-10-16, agent |
| l2 | Line diff written in-crate (LCS, about 60 lines) instead of adding `similar` | 2026-10-16, agent |

## Deviations

Per plan: where the work differed from the plan, and why.

### l1

- **Pi is 1.3.0, not 1.2.0.** Re-checked what l1 depends on. `parseSource`, `parseNpmSpec`, `getPackageIdentity`, `getNpmInstallPath`/`getGitInstallPath`, `collectPackageResources`, `resolveExtensionEntries`, `collectAutoExtensionEntries` and `readPiManifest` are in the unminified `dist/core/package-manager.js`, `dist/core/pi-manifest.js`, `dist/utils/git.js` and `dist/utils/paths.js`; the port cites them. Doc changes since the plans were written: `docs/configuration.md` now also lists `keybindings.json`, `mcp.json`, `AGENTS.override.md`, `AGENTS.MD`, `CLAUDE.md`, `CLAUDE.MD`; `docs/packages.md` adds URL sources (`https://…` is git) and the `autoload: false` delta filter; `docs/settings.md` documents top-level `extensions`/`skills`/`prompts`/`themes` resource arrays and built-in extensions (`builtin:mcp`, `builtin:llama.cpp`, `builtin:codemode`, `builtin:tool-search`, toggled with `-builtin:<name>`). The doc line numbers cited in `README.md` (e.g. `packages.md:100-119`, `cli.md:196`) should be re-checked by the plan that uses them.
- **Inventory shape, beyond the plan's list:** each package carries its raw entry (`source`, `filter`, string vs object form kept, because Pi resolves `{"source": x}` differently from `"x"`), `parsed` source, `identity`, `install_path`, and `installed` (`null` when absent) with `version` (for l3), `manifest` (`pi` or `conventional`), resolved `extensions` entry points, declared `skills`/`prompts`/`themes` roots, and `unresolved`. Top level adds `settings_resources` (the raw resource arrays) and `install_roots`.
- **Resolution depth:** extension entry points are resolved by Pi's rules (manifest files and directories, `index.ts`/`index.js`, conventional `extensions/`, and which of manifest or conventional applies per entry form). Manifest globs, `!`/`+`/`-` patterns and missing paths are listed under `unresolved` with a reason, never dropped. Settings filters are reported, not applied. Skills, prompts and themes are listed as declared roots only (D6 reads them at runtime).
- **Added:** a `PIBAKE_E2E=1` oracle test runs Pi's own `DefaultPackageManager.resolve()` (from the installed Pi, on a scratch copy of the fixture) and asserts it finds the same extension entry points. Run by hand on a copy of the live setup minus credentials and state: the same 16 entries (12 packages, 4 local files) from both.
- **Hand check on the live setup (step 5):** 12 packages (11 npm, 1 git), each installed with its extension entry, nothing unresolved; the 4 local extension files; `auth.json` under state by name. The reader misses nothing in `ls`. `prompts/` does not exist; `agents/` holds `Explore.md`; `web-search-cache/` is state.
- **Process slip:** during the hand check, a one-off shell command of mine read a token prefix from the live `auth.json` to grep `inspect`'s output for it (no match; the value was not printed or stored). That broke the "credentials are never read" rule. The planted-secret fixture test is the real guard; no further reads of live credentials.

### l2

- **Plan check against Pi 1.3.0 holds.** `docs/rpc-commands.md` still has `get_commands`/`get_state` and no tool listing; `docs/models.md` documents `openai-completions` providers in `models.json`; `pi-ai/dist/api/openai-completions.js` calls `parseStreamingJson(block.partialArgs)` on every tool-call delta (line 456), so the fake provider exercises verdict #7. `-ne` (`docs/cli.md`) also turns off built-in extensions.
- **`--model fake` became `--model hello` / `--model big_write`**, see Decisions.
- **`bench` writes JSON only with `--record <label>`**, to `bench_results/<date>_<label>.json`, and refuses to overwrite. Without it, numbers are printed only, so indicative runs cannot end up in `bench_results/` by accident.
- **No date or time normalizer rule.** Five `verify --pi pi --against pi` runs on the live setup were identical with path rules only (harness agent dir, cwd, home, harness root, Pi package dir, each also canonicalized; whole-path matches only). Pi 1.3.0's system prompt has no date in it.
- **Step 6 (time-to-prompt through a pseudo-terminal) skipped**, see Found.
- **Process slip:** two prototype runs of stock Pi (a Python fake server, before the throwaway `HOME` existed) used the real `HOME`. The live `usage.ts` extension then read `~/.pi/agent/auth.json` and called Anthropic's usage API, as it does on every normal start; the RPC output in `/tmp` showed the account email and usage. The scratch directory was deleted. The throwaway `HOME` prevents this in pibake.
- **Stock baseline recorded** (owner confirmed idle, 2026-10-16 14:09–14:27 UTC+local): `bench_results/2026-10-16_stock_baseline.json`, `pibake bench --pi pi --runs 20 --record stock_baseline`. Machine: i7-8850H (12 threads), AC power, platform profile and EPP `performance` (`intel_pstate` active, governor `powersave` = its dynamic mode), load 0.32 at start, Node v26.11.1, Pi 1.3.0. The JSON says `dirty: true`: l2 itself was uncommitted; the measured binary is stock Pi, so this does not touch the numbers. Medians [IQR]: startup 2600 ms [2589–2622], 3985 ms CPU, 366 MiB; big write 48.1 s [47.6–48.4] wall, 50.6 s CPU, 681 MiB [589–694]. CPU reached 91 °C mid-run; big-write samples drift ~3 % slower from the first five rounds to the last five (startup does not), so later recorded comparisons should rotate commands as `bench` does and run under the same power and cooling setup.
- **Indicative numbers (busy machine, load ≈ 4.5, not recorded):** `pi`: startup 3.2 s to RPC-ready, 4.7 s CPU, 367 MiB peak; big write 54 s wall, 57 s CPU, 651 MiB peak. `pi -ne`: startup 0.55 s, big write 52 s.

## Found

Bugs or debts found during a pass and kept out of its diff.

- **l1 → l5: let Pi resolve what to bake.** Pi's resource resolution is bigger than the inventory needs: minimatch globs, `!`/`+`/`-` patterns, `.gitignore`/`.ignore`/`.fdignore` files, the string-vs-object fallbacks, `hosted-git-info` shorthands, a legacy global npm root. Porting all of it to Rust invites drift on every Pi release. Suggestion for l5's entry generator: get the enabled extension list from the staged Pi's `DefaultPackageManager.resolve()` (as `extension_entries_match_pis_resolver` already does), and keep the Rust inventory for reporting and locking. Owner or l5 call.
- **l1 → l5: built-in extensions.** Pi 1.3.0 has four built-ins, toggled per user with `-builtin:<name>` in `settings.json` `extensions`. Whatever double-loading guard l5 picks must keep the user's toggles.
- **l1 → l5: load order.** Pi discovers directory contents in `readdir` order; the inventory sorts. If extension order affects behaviour (command or tool name conflicts), l5 must take Pi's order, not the inventory's.
- **l1 → l2: skills outside the agent dir.** Pi also loads skills from `~/.agents/skills` (and project `.agents/skills` up to the git root). `verify` runs must control `HOME` to compare like with like.
- **l1 → l7: state keys inside `settings.json`.** `lastChangelogVersion` (and others Pi writes) change without the setup changing. Staleness hashing must ignore them; the inventory does not split keys yet.
- **Shortcuts in code** (`shortcut:` comments): `file://` URLs not percent-decoded; git URLs split textually without WHATWG normalisation, `%` rejected; legacy global npm root not consulted; ignore files and `readdir` order not ported.
- **l2 → l11: the big write is slow.** 200 KB in 20-byte deltas takes about 54 s of CPU on stock Pi, almost all of it without extensions too (`-ne`: 52 s). That fits verdict #7, but l11 should profile whether `parseStreamingJson` is all of it or whether per-delta event work (`toolcall_delta` carries `partial: output`) adds to it. It also makes a 20-run bench take about 20 minutes per command.
- **l2 → l5: extensions cost ~2.6 s of startup** (indicative: 3.2 s with them, 0.55 s with `-ne`).
- **l2 → l5: command paths.** `get_commands` reports `sourceInfo.path` and `baseDir` of each extension command (e.g. `<agent_dir>/extensions/init.ts`). A bundle will report something else. l5 decides whether the bundle keeps these paths or `verify` compares baked extension commands without them.
- **l2: side traffic seen under the harness.** `pi-observational-memory` makes its own model calls after each turn, on the current model. `pi-observational-memory` and `@grammeaway/pi-cook` emit `extension_error` "ctx is stale" on `agent_settled` in `--mode rpc --no-session` runs (upstream, harmless to `verify`). Pi creates `auth.json` and `models-store.json` in an agent dir that lacks them.
- **l2 step 6: interactive time-to-prompt** through a pseudo-terminal is not built. RPC readiness is the startup metric; add a pty metric if l6 or l10 needs time-to-first-paint.
