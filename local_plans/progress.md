# Local release builds: progress

Live log for `README.md`'s plans. One row per plan; details under Deviations and Found.

## Status

| Plan | Status | Base | Notes |
|---|---|---|---|
| l1 scaffold, inspect | done | db3648d | Pi 1.3.0 (plan: 1.2.0); Hegel for property tests; Pi-resolver oracle test added |
| l2 bench, verify | planned | | baseline needs an idle machine |
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

## Deviations

Per plan: where the work differed from the plan, and why.

### l1

- **Pi is 1.3.0, not 1.2.0.** Re-checked what l1 depends on. `parseSource`, `parseNpmSpec`, `getPackageIdentity`, `getNpmInstallPath`/`getGitInstallPath`, `collectPackageResources`, `resolveExtensionEntries`, `collectAutoExtensionEntries` and `readPiManifest` are in the unminified `dist/core/package-manager.js`, `dist/core/pi-manifest.js`, `dist/utils/git.js` and `dist/utils/paths.js`; the port cites them. Doc changes since the plans were written: `docs/configuration.md` now also lists `keybindings.json`, `mcp.json`, `AGENTS.override.md`, `AGENTS.MD`, `CLAUDE.md`, `CLAUDE.MD`; `docs/packages.md` adds URL sources (`https://…` is git) and the `autoload: false` delta filter; `docs/settings.md` documents top-level `extensions`/`skills`/`prompts`/`themes` resource arrays and built-in extensions (`builtin:mcp`, `builtin:llama.cpp`, `builtin:codemode`, `builtin:tool-search`, toggled with `-builtin:<name>`). The doc line numbers cited in `README.md` (e.g. `packages.md:100-119`, `cli.md:196`) should be re-checked by the plan that uses them.
- **Inventory shape, beyond the plan's list:** each package carries its raw entry (`source`, `filter`, string vs object form kept, because Pi resolves `{"source": x}` differently from `"x"`), `parsed` source, `identity`, `install_path`, and `installed` (`null` when absent) with `version` (for l3), `manifest` (`pi` or `conventional`), resolved `extensions` entry points, declared `skills`/`prompts`/`themes` roots, and `unresolved`. Top level adds `settings_resources` (the raw resource arrays) and `install_roots`.
- **Resolution depth:** extension entry points are resolved by Pi's rules (manifest files and directories, `index.ts`/`index.js`, conventional `extensions/`, and which of manifest or conventional applies per entry form). Manifest globs, `!`/`+`/`-` patterns and missing paths are listed under `unresolved` with a reason, never dropped. Settings filters are reported, not applied. Skills, prompts and themes are listed as declared roots only (D6 reads them at runtime).
- **Added:** a `PIBAKE_E2E=1` oracle test runs Pi's own `DefaultPackageManager.resolve()` (from the installed Pi, on a scratch copy of the fixture) and asserts it finds the same extension entry points. Run by hand on a copy of the live setup minus credentials and state: the same 16 entries (12 packages, 4 local files) from both.
- **Hand check on the live setup (step 5):** 12 packages (11 npm, 1 git), each installed with its extension entry, nothing unresolved; the 4 local extension files; `auth.json` under state by name. The reader misses nothing in `ls`. `prompts/` does not exist; `agents/` holds `Explore.md`; `web-search-cache/` is state.
- **Process slip:** during the hand check, a one-off shell command of mine read a token prefix from the live `auth.json` to grep `inspect`'s output for it (no match; the value was not printed or stored). That broke the "credentials are never read" rule. The planted-secret fixture test is the real guard; no further reads of live credentials.

## Found

Bugs or debts found during a pass and kept out of its diff.

- **l1 → l5: let Pi resolve what to bake.** Pi's resource resolution is bigger than the inventory needs: minimatch globs, `!`/`+`/`-` patterns, `.gitignore`/`.ignore`/`.fdignore` files, the string-vs-object fallbacks, `hosted-git-info` shorthands, a legacy global npm root. Porting all of it to Rust invites drift on every Pi release. Suggestion for l5's entry generator: get the enabled extension list from the staged Pi's `DefaultPackageManager.resolve()` (as `extension_entries_match_pis_resolver` already does), and keep the Rust inventory for reporting and locking. Owner or l5 call.
- **l1 → l5: built-in extensions.** Pi 1.3.0 has four built-ins, toggled per user with `-builtin:<name>` in `settings.json` `extensions`. Whatever double-loading guard l5 picks must keep the user's toggles.
- **l1 → l5: load order.** Pi discovers directory contents in `readdir` order; the inventory sorts. If extension order affects behaviour (command or tool name conflicts), l5 must take Pi's order, not the inventory's.
- **l1 → l2: skills outside the agent dir.** Pi also loads skills from `~/.agents/skills` (and project `.agents/skills` up to the git root). `verify` runs must control `HOME` to compare like with like.
- **l1 → l7: state keys inside `settings.json`.** `lastChangelogVersion` (and others Pi writes) change without the setup changing. Staleness hashing must ignore them; the inventory does not split keys yet.
- **Shortcuts in code** (`shortcut:` comments): `file://` URLs not percent-decoded; git URLs split textually without WHATWG normalisation, `%` rejected; legacy global npm root not consulted; ignore files and `readdir` order not ported.
