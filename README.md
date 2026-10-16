# pibake

`cargo build --release` for your [Pi](https://pi.dev) setup.

pibake takes a Pi agent directory (the `~/.pi/agent` you already have, or a git repo laid out the same way) and builds it into one executable: Pi, your packages and your extensions, bundled, optimized for that exact setup, and pinned to exact versions.

> **Status:** early. `pibake inspect` works; the rest of the first round is in [`local_plans/`](local_plans/README.md).

## Why

Stock Pi resolves, transpiles and evaluates everything at every launch: 9 MB of bundled chunks, every provider SDK whether you use it or not, and each TypeScript extension through jiti. A built binary does that work once, at build time, and the build knows your whole setup, so it can drop what you never use.

Two uses, one tool:

- **Local release builds.** A faster Pi for your own machine, from your live setup.
- **Shareable setups.** A version-controlled agent directory plus a lockfile, built in CI into the same binary for every dev box and container.

Pi itself is not forked. pibake builds released Pi packages through Pi's own entry point, so the behaviour is Pi's.

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
| `pibake build` | Builds the binary. Content-hashed caches skip unchanged work |
| `pibake verify` | Diffs the binary's tools, commands and system prompt against stock Pi on the same setup |
| `pibake bench` | Startup time, memory, and CPU over a streamed large write, against stock Pi |
| `pibake install` | Copies the binary to `~/.local/bin` |
| `pibake init` (planned) | Turns a live setup into a repo-mode directory, without credentials or state |

### What the binary reads at runtime

1. **Baked:** Pi, your packages' extension code, and build-time decisions. Read-only.
2. **The agent directory:** auth, sessions, settings, skills and prompts, shared with stock Pi, so you can switch between the two freely.
3. **Project `.pi/settings.json`**, as Pi applies it today.

A binary built from a setup that has since changed says so in one line at startup.

## How it builds

```
pibake.lock ─► stage ─► passes ─► bundle ─► backend ─► verify + bench
```

- **Stage:** a clean, pinned install of Pi and your packages.
- **Passes:** optimizations on the bundled code that only make sense when the whole setup is known, such as dropping unused providers and highlight.js languages. Written in Rust on [oxc](https://oxc.rs).
- **Bundle:** one file, with every extension imported directly instead of discovered at runtime.
- **Backend:** Node's [single executable applications](https://nodejs.org/api/single-executable-applications.html), with V8's code cache first and a startup snapshot later. Node is the runtime Pi targets, so there is no runtime drift. Other backends ([Pi-Bolt](https://github.com/opensec-git/Pi-Bolt)'s ahead-of-time compiled JavaScriptCore, `deno compile`) can plug in at the same point later.

Every optimization is measured against stock Pi by `pibake bench`, and kept only if the numbers say so.

## Not goals

- A Pi fork, or a reimplementation of Pi in another language.
- A config format of its own for anything Pi already configures.
- Making the model faster. That is the provider's.
