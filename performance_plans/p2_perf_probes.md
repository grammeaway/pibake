# P2: `perf` probes for built executables

## Finding

Every profile so far (l8, l10, l11) used `--cpu-prof`. It has two blind spots:

- **It sees JavaScript only.** Snapshot and code-cache deserialisation, compilation, GC, ICU, string conversion and page faults all show up as `(program)` or `(garbage collector)`.
- **It changes V8's flags, so V8 drops the code cache** (l8 Found). The profile is then not of the backend we ship. `src/sea.rs` already notes the same thing for `--hash-seed`.

Probe on the installed `~/.local/bin/pi-baked`, `--help`, 2–3 runs per row (2026-10-17, load 0.5, indicative):

| Run | user task-clock | `instructions:u` |
|---|---|---|
| plain | 545–574 ms | 2.90–2.91 G |
| `NODE_OPTIONS=--perf-basic-prof` | 900–934 ms | 5.12–5.18 G |
| `--perf-basic-prof-only-functions` | 900–906 ms | 5.12 G |
| `--interpreted-frames-native-stack` | 840 ms | 4.85 G |
| `--max-semi-space-size=16` (control) | 549–553 ms | 2.89–2.90 G |

- **The perf flags cost what losing the code cache costs.** Any of them adds 290–380 ms and about 2 G instructions, close to l6's +280 ms without a code cache. The heap-size flag adds nothing. So you can't profile a shipped executable by passing flags at runtime. The flags have to be there when the cache is built.
- **`instructions:u` is a quieter A/B number than time.** Its run-to-run spread is about 0.5%, against about 5% for task-clock. l7, l8 and l9 each ended with "IQRs overlap". viagoDB uses `instructions:u` for exactly this.
- **`perf record -e cycles:u`, plain run.** By DSO: 71.5% the executable, 11.8% JIT, 6.8% `libicuuc`, 6.6% libc. Within ICU, the time goes to `UnicodeSet` and `EmojiProps` building (see Candidates in the README). simdutf UTF-8 → UTF-16 conversion takes about 1.3% (p3).
- **The executable's Node frames have no names.** Arch's `nodejs 26.11.1-1` is stripped, and the SEA keeps Node's build ID (`4da26f6b…`). `perf` is built with debuginfod support, but `debuginfod.archlinux.org` did not answer within 60 s during the probe.
- `perf_event_paranoid` is 2, so `:u` events on our own processes work without root, as in viagoDB.

viagoDB's setup (`viagodb/README.md` → Profiling) has four parts:

1. a `profiling` cargo profile (release plus line tables);
2. an in-process probe binary;
3. `perf stat` and `perf record`, gated with `--control fifo` to the measured phase;
4. `instructions:u` as the A/B counter.

For pibake, the counterpart of (1) is an executable built with the perf flags. The bench harness already does (2)'s job. pibake's own Rust is not worth profiling: `pibake bundle` with every pass takes about 2.4 s and peaks at about 11 MiB, and `--build-sea` dominates the build (l6).

## Change

- **`pibake build --profiling`** writes `target/pibake/bin/<name>-profiling`. It passes `--perf-basic-prof-only-functions --interpreted-frames-native-stack` twice: to the `node --build-sea` that makes the code cache or snapshot, and in the SEA config's `execArgv`. The cache and the run then have the same flags. The flags go into the build's content hash, so the profiling and plain builds never share a cache entry.
- **`pibake bench --perf-stat`** runs each command under `perf stat -x, -o <file> -e instructions:u,cycles:u,page-faults:u --` and records each counter as one more series per metric (startup, big write). It fails with one line if `perf` is missing or can't open the events.
- **A `Profiling` section in the README**, in viagoDB's style: build, `perf stat`, `perf record -g`, `perf report --sort dso,sym`, how to get Node's symbols, and the flag/cache pitfall above.

## Steps

1. **Prototype by hand, no pibake code.** Edit a kept `sea.json` and run `--build-sea` directly. Check:
   - With the flags on both sides, the profiling executable's `instructions:u` for `--help` and for RPC startup are within 3% of the plain build, on both backends. If not, the cache is still being rejected. Remove flags one at a time until it is accepted.
   - `perf report` names JS functions on both backends. Does V8 write snapshot-deserialised code to the map?
   - Node's symbols resolve. Try Arch debuginfod first (`DEBUGINFOD_URLS`, `perf buildid-cache`). If that fails, try an unstripped official nodejs.org build of the locked version, and note that it is a different executable. Also check whether the Arch build keeps frame pointers (`--call-graph fp` against `dwarf`).
   - Wrapping with `perf stat` leaves the startup medians (ms, CPU, RSS) within noise over 5 runs. `wait4` then reaps `perf`, and `ru_maxrss` covers its reaped child too.
2. **Implement** `--profiling` (`src/main.rs`, `src/sea.rs`, hash input) and `--perf-stat` (`src/bench.rs`; the prefix goes on `harness.command`).
3. **README** Profiling section.
4. **Breakdown:** one `perf record` per backend (stock, `sea_codecache`, `sea_snapshot`) of RPC startup and of one big write, with the top DSOs and symbols logged in this plan's progress. Indicative, not recorded. It feeds p1's candidates and p3.

## Completion

- [x] ~~The profiling executable's startup `instructions:u` is within 3% of the plain one, on both backends.~~ **Wrong criterion.** Baked flags keep the cache, which is what this was meant to show. Runtime flags cost +2.0–2.3 G instructions on `--help`, baked ones +0.6 G. The rest is the flags' own work, not a rejected cache: map writing, line ends over the 20 MB bundle, and built-in modules that lose Node's embedded cache. See `progress.md` → Tooling. Profiling builds are for names, not shares.
- [x] `perf report` names JS functions and, with the official node binary, Node/V8 C++ symbols, and the README says how.
- [x] `bench --perf-stat` records `*_minstr` per run in the JSON. A unit test covers the CSV parser, including `<not supported>`. There is no E2E test, since CI runners may not allow perf events.
- [x] `bench` without `--perf-stat` measures as before. The table gains three `-` rows.
- [x] `cargo fmt --check`, clippy and every test green, E2E included.
- [x] Step 4's breakdown is in `progress.md` → Probe round.

## Non-goals

- `--control fifo` gating. Startup is the whole process, and a big write is almost all streaming. Add gating when a phase needs isolating.
- A `profiling` cargo profile for pibake itself. Its own work is about 2.4 s and 11 MiB.
- Flamegraph tooling, and any optimisation.
