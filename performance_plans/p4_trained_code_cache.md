# P4: A trained code cache

## Finding

From the probe round (`progress.md` → Where startup goes, Trained code cache):

- **Today's code cache leaves most of startup's compile work to launch.** `--build-sea` creates the cache right after compiling the main, so it holds top-level code only. Every function that startup calls is parsed and compiled at launch: **40 % of the code-cache executable's main thread** (parse 32 %).
- **V8 caches what has run, if asked late.** `vm.Script#createCachedData()` serialises every function compiled so far, not just the top level. Taken at the end of three chained harness runs (RPC startup, interactive startup, a 1 KiB print turn), the cache grows from 2.5 to 7.6 MB.
- **A SEA can consume such a cache.** Its main is a 7-line CommonJS loader. The loader compiles the bundle from a SEA asset with the trained cache as `cachedData`, and calls it.

  The probe, 5 runs, `--perf-stat`, same lock (indicative):

  | | `sea_codecache` | `sea_snapshot` | trained, ASCII bundle (p3) |
  |---|---|---|---|
  | RPC startup | 450 ms | 325 ms | 325 ms |
  | interactive ready | 499 ms | 386 ms | 368 ms |
  | print turn | 556 ms | 421 ms | 395 ms |
  | startup peak RSS | 181 MiB | 213 MiB | 170 MiB |

  Without p3 the trained build peaks at 196 MiB, because the source becomes a two-byte string.
- **The cache's validity is V8's to judge.** V8 checks the source length, V8 version and flag hash, and on a mismatch it compiles from source. The result is correct, only slower. The probe trained under plain `node` (the same binary the SEA embeds) and the SEA accepted the cache.
- **Training is not reproducible byte for byte.** Two trainings gave caches 144 bytes apart.

## Change

A third backend, `sea_trained`. It becomes the default only after step 6's owner call.

- **Bundle:** the CommonJS format the snapshot backend already builds, with:
  - a prologue that extracts at launch, as `prologue.mjs` does;
  - esbuild's banner and footer wrapping the whole output as `(function (exports, require, module, __filename, __dirname) { … })`. Then the loader compiles the asset string as it is, and no 20 MB concatenation is needed.
  - The snapshot-only parts stay out: `snapshot_safety`, the lazy builtins, deferred signals.
- **Loader** (`src/bundle/loader_trained.cjs`, the SEA main, `useCodeCache: true` for itself): compiles `getAsset("bundle.cjs", "utf8")` with `cachedData` = `getAsset("cache.bin")` and calls it. A rejected cache gives a correct, slower launch.
- **Training at build time:** before `--build-sea`, `build` runs the bundle under plain `node` with a build-only training loader (`src/bundle/train.cjs`, like `trace.cjs`), through the bench's `Harness` and fake provider. It runs three times: RPC startup, interactive startup in a pty, a `hello` print turn. Each run loads the previous cache and writes a new one at exit.
  - The runs reuse `bench`'s run code instead of a copy.
  - The result is stored under `target/pibake/train/<bundle hash>/cache.bin` and reused while the bundle is unchanged. Rebuilds are then byte-identical, and pay for training once.
- **Acceptance check:** after `--build-sea`, `build` runs the executable once with `PIBAKE_CACHE_CHECK=1`, where the loader exits right after compiling, with status 3 if V8 rejected the cache. A rejection fails the build.

## Steps

1. **Untrained first:** the `sea_trained` bundle flavour, the loader, and a cache made right after compiling. `verify` same against stock on the live setup. Bench parity with `sea_codecache` (`--runs 5 --perf-stat`), so that later gains are the training's.
2. **Training** (`src/sea.rs` or a new `src/train.rs`), the per-bundle cache directory, and the acceptance check.
3. **Measure** against step 1 and both old backends: `bench --runs 10 --perf-stat --big-write-kib 200`. The probe says `startup_minstr` −20 %, `ready_ms` −25 %; the big write must not regress.
4. **Coverage check:** whether training also on `--help`, or with `PI_TIMING` code paths, changes anything. Keep the three runs unless a fourth earns its build time.
5. **Tests:**
   - unit tests for the build steps, with a fake `node` as `sea.rs` does;
   - `PIBAKE_E2E=1`: the fixture builds with `sea_trained`, the acceptance check passes, `verify` is the same, and a second build reuses the training (no harness run, the same executable bytes);
   - a deliberate break: an executable whose cache is from another bundle fails the acceptance check.
6. **Gate:** ask the owner for an idle machine. Record `bench --runs 20 --perf-stat --record p4_trained` with `sea_codecache`, `sea_snapshot` and `sea_trained` from one lock. Then put O3 and O4 (README) to the owner with the numbers.

## Completion

- [ ] `backend = "sea_trained"` builds, passes `verify`, and its acceptance check, on the live setup and the fixture.
- [ ] A rebuild with an unchanged bundle reuses the training and is byte-identical.
- [ ] Recorded bench in `bench_results/`.
- [ ] `cargo fmt --check`, clippy and every test green, E2E included.

## Non-goals

- Combining with the snapshot. The probe's snapshot builder rejected the trained cache, because `mksnapshot` runs V8 under other flags. Revisit only if O3 keeps the snapshot.
- Training on real sessions or user traffic. The three harness runs cover startup, which is what the cache is for.
- Merging caches from separate runs. Chaining does the same.
