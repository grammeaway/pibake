# pibake

`cargo build --release` for your [Pi](https://pi.dev) setup.

pibake takes a Pi agent directory (the `~/.pi/agent` you already have, or a git repo laid out the same way) and builds it into one executable: Pi, your packages and your extensions, bundled, optimized for that exact setup, and pinned to exact versions.

> **Status:** early. `pibake inspect`, `lock`, `stage`, `verify` and `bench` work; the rest of the first round is in [`local_plans/`](local_plans/README.md).

## Why

Stock Pi resolves, transpiles and evaluates everything at every launch: 9 MB of bundled chunks, every provider SDK whether you use it or not, and each TypeScript extension through jiti. A built binary does that work once, at build time, and the build knows your whole setup, so it can drop what you never use.

Two uses, one tool:

- **Local release builds.** A faster Pi for your own machine, from your live setup.
- **Shareable setups.** A version-controlled agent directory plus a lockfile, built in CI into the same binary for every dev box and container.

Pi itself is not forked. pibake builds released Pi packages through Pi's own entry point, so the behaviour is Pi's.

## Install

Linux only for now (x86_64 and aarch64; see [Roadmap](#roadmap)). Download a static binary from [Releases](https://github.com/grammeaway/pibake/releases) and put `pibake` on your `PATH`, or build from source with `cargo install --path .`.

pibake drives the tools Pi already uses, so it needs `node` (25.5 or newer), `npm` and `git` on `PATH`.

## The flow

Your config stays in Pi's own formats. `settings.json`, packages, extensions, skills, `models.json` and `AGENTS.md` mean what Pi's docs say they mean. pibake adds two files:

| File | What |
|---|---|
| `pibake.toml` | Build settings Pi has no concept of: backend, targets, optimization passes, trimming |
| `pibake.lock` | Pinned Pi version, Node version, git commits, and npm's own `package-lock.json` |

Any agent directory runs on stock Pi as-is (`PI_CODING_AGENT_DIR=./my-setup pi`), which is your debug build. `pibake build` is the release build of the same directory.

**Local mode** points at your live setup by reference:

```toml
[source]
agent_dir = "~/.pi/agent"
```

**Repo mode** (planned) is a git repo that *is* the agent directory:

```toml
[source]
agent_dir = "."
```

### Commands

| Command | Does |
|---|---|
| `pibake inspect` | Lists what the setup contains, and what is config versus state |
| `pibake lock [--update]` | Resolves and pins versions. Without `--update`, a second run changes nothing |
| `pibake stage` | Installs the lock into `target/pibake/stage/<hash>/`, reused while the lock and local extensions are unchanged |
| `pibake bundle` | Stages, then bundles Pi and every extension into `target/pibake/bundle/<hash>/pi.mjs` (`node` runs it), with a per-extension compatibility report |
| `pibake build` | Checks the lock for drift, bundles, then builds `target/pibake/bin/<name>` (`[build] name`, default `pi-baked`): one executable with Node inside. Content-hashed caches skip unchanged work. `--profiling` builds `<name>-profiling` for `perf` (see [Profiling](#profiling)) |
| `pibake verify` | Diffs the binary's tools, commands and system prompt against stock Pi on the same setup |
| `pibake bench` | Startup and session scenarios against stock Pi: RPC and interactive startup (first paint, ready), an RPC session's first turn 1 s after ready (prompt to first token), a streamed large write in print mode and in the TUI (`--big-write-kib`, default 200; 0 skips both), a long Markdown answer and 8 MiB of `bash` output in the TUI, and 40 RPC turns (CPU of early against late turns, memory at the end). `--scenarios` picks some (default: all). `--perf-stat` also counts each run's instructions |
| `pibake install` | Builds, then copies the binary to `~/.local/bin/<name>` |
| `pibake init` (planned) | Turns a live setup into a repo-mode directory, without credentials or state |

### What the binary reads at runtime

1. **Baked:** Pi, your packages' extension code, and build-time decisions. Read-only.
2. **The agent directory:** auth, sessions, settings, skills and prompts, shared with stock Pi, so you can switch between the two freely.
3. **Project `.pi/settings.json`**, as Pi applies it today.

A binary built from a setup that has since changed (packages, their versions or git commits, local `extensions/` files, the global Pi) says so in one line on stderr at startup, naming what changed. `PIBAKE_NO_STALENESS_CHECK=1` turns that off.

## How it builds

```
pibake.lock ─► stage ─► passes ─► bundle ─► backend ─► verify + bench
```

- **Stage:** a clean, pinned install of Pi and your packages.
- **Passes:** optimizations that only make sense when the whole setup is known, such as dropping unused provider APIs and highlight.js languages. Written in Rust on [oxc](https://oxc.rs), they rewrite the few module sources they target before bundling, so the bundler leaves out whatever they cut. Each one fails the build when Pi's code no longer has the shape it expects. `[build] passes` lists the passes to run (default: all but the opt-in `keep_alive_http`; `[]` turns them off), and `[trim] highlight_languages` lists the highlight.js languages code blocks are coloured in. Others render as plain text, as an unknown language does in stock Pi. `[trim] apis` lists the provider APIs (`anthropic-messages`, `openai-completions`, …) whose stream code and SDKs stay in. By default these are the ones the setup's default provider, `enabledModels`, `models.json` and extensions use. Every provider still logs in and lists its models; a request through a dropped API fails with one line naming the setting, and so does startup when the setup asks for one. `incremental_tool_json` makes the providers parse a streamed tool call from each new delta, instead of re-parsing the whole call on every delta, with the same result at every step. `incremental_markdown` does the same for a streamed answer's Markdown: it lexes and renders only from the last block a delta could change, keeping the rest, with every frame drawn as stock Pi draws it. `lazy_http_dispatcher` creates Pi's HTTP dispatcher (undici, the proxy agent, the global `fetch`) on the first request instead of at startup, with the proxy and timeout settings stock Pi would have used. `prewarm_first_turn` then makes that dispatcher and evaluates the current model's provider SDK once RPC or interactive mode is ready, and again after a model change, each step in its own macrotask, so a session's first prompt does not wait for them: the same modules and dispatcher, only sooner. `keep_alive_http` (opt-in, as it changes stock Pi's network behaviour) keeps an idle connection for 120 s instead of undici's 4 s, so a prompt after the user's think time skips a new TCP and TLS handshake (~60 ms to `api.anthropic.com`); a server's own `Keep-Alive: timeout=` still wins. It applies to every origin Pi's `fetch` reaches, extensions' included: one that closes idle connections sooner without that hint makes the next request fail with `other side closed`, and Pi retries it after a back-off. `lazy_modules` evaluates marked and pi-tui's text utilities, with the editor and markdown modules that use them, on first use, so RPC and print mode never build their regexes and segmenters; it does not apply to `sea_snapshot`, which holds them evaluated. `ascii_bundle` runs on the bundle itself: it escapes the non-ASCII characters esbuild leaves in comments and regex literals, so Node holds the executable's code as one byte per character instead of two.
- **Bundle:** one file holding Pi and every extension. Pi still discovers and orders extensions as usual; the bundle answers its loader with the baked code instead of transpiling from disk, and anything it did not bake (a project extension, one added later) loads as in stock Pi.
- **Backend:** Node's [single executable applications](https://nodejs.org/api/single-executable-applications.html). Node is the runtime Pi targets, so there is no runtime drift. `[build] backend` picks one of three:
  - `sea_codecache` (the default) ships an ES module with V8's code cache.
  - `sea_snapshot` ships a V8 startup snapshot. The snapshot is taken after every module has been evaluated at build time, so a launch skips that work.
  - `sea_trained` (opt-in) ships a CommonJS bundle with a code cache trained at build time. That cache holds every function startup and a print turn compile, where `sea_codecache`'s holds only the top level. Training runs the setup, its extensions included, under `node` in a throwaway home with a local fake provider, offline. It is kept under `target/pibake/train/` and reused while the bundle is unchanged, so a rebuild is byte-identical. The build fails if the new executable's V8 rejects the cache.

  For `sea_snapshot`, pibake defers what module code would otherwise freeze at build time: environment and terminal reads, ICU objects, signal listeners, and the HTTP stack. Those run at launch instead. A traced build fails, naming the module, on anything else read at load. The files Pi reads from disk are extracted to the user's cache at build time, and the executable keeps using that path. Other backends ([Pi-Bolt](https://github.com/opensec-git/Pi-Bolt)'s ahead-of-time compiled JavaScriptCore, `deno compile`) can plug in at the same point later.

Every optimization is measured against stock Pi by `pibake bench`, and kept only if the numbers say so.

### Profiling

`perf` (Linux) can see what `--cpu-prof` cannot: V8's compiler, deserialiser and GC, ICU, and Node's own C++. User-space events need no root at `perf_event_paranoid` 2.

- **Count, don't time.** `pibake bench --perf-stat` runs each command under `perf stat` and records `<scenario>_minstr` for each scenario, e.g. `startup_minstr` (millions of user-space instructions). On a laptop their spread is about ±0.3 %, against several percent for wall time, so small A/B effects show. Times then include `perf`'s own start, ~10 ms.
- **Name JS frames with a profiling build.** `pibake build --profiling` writes `<name>-profiling`, built with `--perf-basic-prof` (and `--interpreted-frames-native-stack` on the code-cache backend) both at build time and in the executable. Passing those flags at runtime instead (`NODE_OPTIONS`) makes V8 reject the code cache, and you profile a cold start. The flags have costs of their own: map writing, and line-end computation over the 20 MB bundle. So read a profiling build for names, not for shares.

  ```bash
  perf record -e cycles:u -g -o pi.data -- target/pibake/bin/pi-baked-profiling --help
  perf report -i pi.data --no-children --sort dso,sym
  ```

  To profile a bench run, wrap the command: `pibake bench --pi "perf record -e cycles:u -g -o /tmp/run.data -- <executable>"` (each run overwrites the file, so use `--runs 1` or a wrapper script that names files per run). The session scenario reads its per-turn CPU from the busiest process under the command, which is Pi under a `perf` wrapper.
- **Node's own symbols.** Distribution builds of `node` are usually stripped, and the executable keeps their build ID. Either fetch the debug file through debuginfod (Arch: `DEBUGINFOD_URLS=https://debuginfod.archlinux.org`, ~3 GB), or build with the official nodejs.org binary of the locked version first on `PATH`: it ships unstripped. It is a different build (static OpenSSL and ICU), so use it to find where time goes, and the usual build for the numbers.

## Not goals

- A Pi fork, or a reimplementation of Pi in another language.
- A config format of its own for anything Pi already configures.
- Making the model faster. That is the provider's.

## Roadmap

- **macOS** (x86_64, aarch64): pibake itself should build there, but is untested. The executables it builds need ad-hoc signing (`codesign --sign -`) after `--build-sea` before macOS will run them.
- **Windows** (x86_64, aarch64): needs the Unix-only parts ported: process groups and `wait4` in the harness and bench, symlinks in staging and bundling, and file modes.
- **Repo mode and `pibake init`:** a version-controlled agent directory built in CI, as described above.

## Acknowledgements

pibake is glue around other people's open source work. It builds on:

| Project | Role in pibake | Source | License |
|---|---|---|---|
| [Pi](https://pi.dev) | The coding agent being built; baked into every executable | [earendil-works/pi](https://github.com/earendil-works/pi) | MIT |
| [Node.js](https://nodejs.org) | Runtime, and single executable applications; embedded in every executable, V8 included | [nodejs/node](https://github.com/nodejs/node) | MIT, plus bundled dependencies' licenses |
| [npm](https://www.npmjs.com) | Resolves and installs the lock | [npm/cli](https://github.com/npm/cli) | Artistic-2.0 |
| [esbuild](https://esbuild.github.io) | Bundles Pi and the extensions | [evanw/esbuild](https://github.com/evanw/esbuild) | MIT |
| [oxc](https://oxc.rs) | Parses and analyses the sources the passes rewrite | [oxc-project/oxc](https://github.com/oxc-project/oxc) | MIT |
| [jiti](https://github.com/unjs/jiti) | Pi's extension loader; the bundle answers it with baked code, and anything not baked still loads through it | [unjs/jiti](https://github.com/unjs/jiti) | MIT |
| [highlight.js](https://highlightjs.org) | Pi's syntax highlighter, trimmed by `trim_highlight` | [highlightjs/highlight.js](https://github.com/highlightjs/highlight.js) | BSD-3-Clause |
| [Git](https://git-scm.com) | Pins and fetches git packages (run as a command, not shipped) | [git/git](https://github.com/git/git) | GPL-2.0 |
| [Hegel](https://github.com/hegeldev/hegel-rust) | Property-based tests | [hegeldev/hegel-rust](https://github.com/hegeldev/hegel-rust) | MIT |

Rust crates: [anyhow](https://github.com/dtolnay/anyhow), [clap](https://github.com/clap-rs/clap), [libc](https://github.com/rust-lang/libc), [serde](https://github.com/serde-rs/serde) and [serde_json](https://github.com/serde-rs/json), [sha2](https://github.com/RustCrypto/hashes), [tempfile](https://github.com/Stebalien/tempfile) and [toml](https://github.com/toml-rs/toml), all MIT OR Apache-2.0. `Cargo.lock` lists the full dependency tree.

An executable built by pibake contains Node.js, Pi and your packages, each under its own license. Check those licenses before you share the executable.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.
