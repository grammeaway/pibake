# P8: Render a streaming answer incrementally

## Finding

From p7's Finding and `progress.md` → Session probes:

- **Each frame of a streaming answer costs the whole answer so far.** The assistant message makes a new `Markdown` for each update. On render, `Markdown` lexes the full text with marked, renders each block token (`renderToken(token, contentWidth, nextToken?.type)`), wraps, pads and flattens every line (pi-tui 1.3.0 `dist/components/markdown.js`, `render`). Its `cachedTokens` hit only on an unchanged text.
- **Probe:** `Markdown` alone takes 36.7 s of CPU for a 60 k-character answer drawn every 24 characters, and 4.0 s for 20 k. Quadratic.
- **Pi-Bolt fixed this in Pi's own code, not in its runtime.** It keeps lexed tokens for the last 8 sources and re-lexes only from the last two blocks. A block's end depends on the line after it. It caches rendered lines per block token. Its tmux bench: 60 k characters in 2.6 s of CPU against 38.7 s for stock Pi on Node. The same Pi changes on stock Bun's JIT land within 3 % of its AOT build (2.98 s against 3.05 s, Linux ARM64). It reports line-for-line, colour-for-colour identical screens (`bench/e2e_screen.py`).
- **Its guards:**
  - a full lex when a link reference definition is present, since that changes earlier blocks;
  - a check that the block tokens' `raw` lengths add up to the source;
  - a full lex once the message is complete.

## Change

A pass, `incremental_markdown`, sends `Markdown.render`'s lex and per-block render through `pibake:markdown` (`src/bundle/markdown.mjs`), matched by exact text as l11's sites are. The module keeps a bounded, module-level cache, since components do not survive updates:

- **Lex:** if the text extends a cached source, keep the cached block tokens except the last two, and lex the rest from the second-to-last block's offset. Fall back to stock's full lex when any of these holds:
  - the raws don't add up;
  - a construct from step 1's list is present (link reference definitions at least);
  - `options.transform` is set.

  `trimPartialClosingFences` runs on the result, as stock does.
- **Render:** reuse the wrapped, padded lines of a block whose `raw`, next token type, width, padding, theme and background are unchanged. Render the rest with stock's own `renderToken` and line code.
- Anything else goes through stock's path.

Where it lives is the owner's call (O6): a pibake pass, or an upstream PR to pi-tui, which stock Pi benefits from too.

## Steps

1. **Map marked's cross-block constructs:** which Markdown, when appended, changes tokens before the last two blocks? Candidates: link reference definitions, setext headings, lazy continuation of lists and quotes, loose/tight lists, open fences, `$$` math (`options.transform`). Each one either stays inside the two re-lexed blocks (shown by the property test) or goes on the fall-back list.
2. **O6:** take the probe, the map and p7's `long_answer` numbers and profile to the owner.
3. **Implement** the module and the pass (or the PR).
4. **Tests:**
   - a Hegel property test through `node`. Generate Markdown documents from a grammar (the step 1 constructs, code fences in highlighted languages, tables, wide and CJK characters, tabs), and feed each to the incremental path in random deltas. After every delta, its lines must equal a fresh stock `Markdown`'s render of the same text, at a generated width;
   - unit tests for the fall-backs;
   - `verify` same.
5. **Measure:** `bench --runs 10 --perf-stat`. `long_answer_cpu_ms` should fall several-fold (Pi-Bolt: ~15× at 60 k), and once by hand at 60 KiB, growth should be close to linear. Startup and the other scenarios must not move.
6. **Gate:** ask the owner for an idle machine. Record `bench --runs 20 --perf-stat --record p8_markdown` with the pass on and off.

## Completion

- [x] The property test is green over the step 1 grammar. Every fall-back has a test that fails without it (E2E, deterministic documents and the property test; mutation-checked, `progress.md`), not a unit test: the guards are only meaningful against stock's render.
- [x] `verify` same on the live setup and the fixture.
- [x] Recorded bench in `bench_results/`.
- [x] `cargo fmt --check`, clippy and every test green, E2E included.

## Non-goals

- The TUI's frame diff over the whole transcript (Pi-Bolt's "stable render"): low on 1.3.0 (README Candidates).
- The other session scenarios. p7 ranks them for the next round.
- Changing what is drawn. Any difference from stock's lines is a bug.
