# P3: An ASCII bundle

## Finding

Measured in the probe round (`progress.md` → Memory):

- **Node encodes the SEA main as UTF-16 unless it is pure ASCII.** The main reaches V8 through `StringBytes::Encode(UTF8)` (Node v26.x `src/string_bytes.cc`):
  - pure ASCII becomes an external one-byte string;
  - its Latin-1 fast path stops at 1 MiB, so a 20 MB source with any non-ASCII character becomes UTF-16. Latin-1 is not enough.
- **Today's code-cache executable holds the 20.6 MB bundle as a 41 MiB external two-byte string**, out of 184 MiB peak. The snapshot holds the same string on its heap.
- **What keeps the bundle non-ASCII:** 1,446 characters on 285 lines (live setup). esbuild's default `charset: ascii` escapes strings, templates and identifiers. By its own docs it escapes neither comments nor regex literals:
  - 248 lines are comment-only;
  - about 35 are regex literals (`[-–—]`, `[f∫∑]`, `🔗`, CJK words);
  - one is a legal comment.
- **What ASCII saves:** escaping every non-ASCII character crudely cut the code-cache executable's peak RSS from 184 to 172 MiB, with `instructions:u` unchanged. Under p4's trained cache, the ASCII bundle is the difference between 196 and 170 MiB.
- The same string is read by Node's `getAsset(…, "utf8")` (p4's loader). The rule there is the same: ASCII gives a one-byte string, anything else UTF-16.

## Change

Add a rewrite of esbuild's output, `ascii_bundle`, before `--build-sea`. It is the first rewrite after bundling; l8's passes all run before esbuild. It parses the bundle with oxc and touches only spans that hold a non-ASCII character:

- **Comments:** each such character becomes its `\u{…}` text. That keeps license notices readable. Comments have no semantics, except in `Function.prototype.toString`, which shows the change.
- **Regex literals:** with `u` or `v`, write `\u{…}`. Without them, write `\uXXXX`, and a surrogate pair as two escapes. These are the same code units, so the meaning is the same in every mode, inside classes and `\q{…}` too.
- **Afterwards,** fail the build if any non-ASCII byte is left, naming its line. That would be a template's `raw`, or an identifier esbuild missed.

It is listed in `[build] passes` (default on), so it can be turned off like the rest.

## Steps

1. **Implement** `src/passes/ascii_bundle.rs`, called from `src/bundle.rs` after esbuild, for both bundle formats.
2. **Tests:**
   - unit tests: a comment, a regex with and without `u`/`v`, an astral character in a class, strings left alone;
   - a Hegel property test through `node`: for generated patterns over a mix of ASCII, BMP and astral characters, and generated inputs, the original and rewritten regexes give the same `exec` results in every flag mode;
   - E2E: `verify` same on the fixture setup, and the bundle is pure ASCII.
3. **Measure** both backends with the pass on and off: `bench --runs 10 --perf-stat`. Peak RSS should drop by about 12 MiB (code cache) and about 20 MiB (snapshot); `*_minstr` should stay within 1 %.
4. **Gate:** ask the owner for an idle machine. Record `bench --runs 20 --perf-stat --record p3_ascii` for both backends, with the pass on and off.

## Completion

- [x] The rewrite and its tests are green; `verify` same on the live setup, both backends.
- [x] A recorded bench in `bench_results/` (`2026-10-17_p3_ascii.json`). **Deviation:** code-cache startup instructions +1.4 %, not within 1 % (`progress.md` → p3).
- [x] `cargo fmt --check`, clippy and every test green, E2E included.

## Non-goals

- Minifying. `minifyWhitespace` saved another 11 MiB on top of ASCII in the probe. That is a separate candidate (README), with its own `toString` and stack-trace questions.
- Rewriting Pi's sources before esbuild. The bundle is the one place where all of them meet.
