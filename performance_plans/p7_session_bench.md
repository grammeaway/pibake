# P7: Session scenarios in the bench

## Finding

- **`bench` measures startup and one big write in print mode. What a session costs is unmeasured.** p1's pty runs exit after `init()` (`PI_STARTUP_BENCHMARK=1`). The big write is `-p`, which renders no TUI. Nothing runs a long answer, large command output, or more than one turn.
- **Pi 1.3.0 has work that is redone on every frame or every turn, and grows with what came before.** One case is measured here; the rest come from a source diff of Pi-Bolt 0.9.4 against Pi v1.3.0 (`progress.md` → Session probes) and Pi-Bolt's own numbers, taken against Pi 1.1.0 and 1.2.0:

  | Scenario | Redone | Growth | Evidence |
  |---|---|---|---|
  | Long streamed answer | Lex and render the whole Markdown per frame | Quadratic | **Measured:** 60 k chars, 36.7 s CPU, component alone. p8 |
  | Large `write` in the TUI | Redraw the tool call per delta; highlight the whole file again when the call completes | Linear per delta | l11 fixed the parse under it. Unmeasured |
  | Large bash output | Rebuild the output view from every line per chunk; truncation splits and joins the whole output | Quadratic | Pi-Bolt: 32 MB, 5.1 → 1.7 s CPU |
  | Long session | Session path walked from the root 5–8× per tool turn; the token estimate; `structuredClone` of the context per request (`pi-btw` handles `context`, so the live setup always pays it) | Linear per turn | Pi-Bolt: stock Node at 482–572 MB own memory after 40 prompts |

- **The transcript's own frame cost is small on 1.3.0.** Re-rendering 400 cached messages (17,600 lines) through `Container.render` takes 0.19 ms; the alt screen's post-processing only touches the visible rows. A turn-by-turn scenario does not need the TUI.

## Change

`bench` gains four session scenarios per command and round. The fake provider gets a script per scenario; each run checks its own end state, so a run that broke early fails rather than passes as fast.

| Scenario | Mode | Script | Ends when | Metrics |
|---|---|---|---|---|
| `long_answer` | pty | A fixed Markdown answer (headings, paragraphs, lists, a table, code in three or four highlighted languages), 20 KiB, 24 characters per delta every 20 ms (Pi-Bolt's `bench/long_answer.py` pace) | The screen has shown the answer's last line | `long_answer_cpu_ms`, `long_answer_peak_rss_mib` |
| `tui_write` | pty | The existing `big_write`, `--big-write-kib` | The screen has shown the closing text; the file matches | `tui_write_cpu_ms`, `tui_write_peak_rss_mib` |
| `big_output` | pty | A `bash` call printing 8 MiB of known lines (`seq`), then a closing text | The screen has shown the closing text | `big_output_cpu_ms`, `big_output_peak_rss_mib` |
| `long_session` | RPC, session file kept | 40 prompts. Each turn `read`s a 16 KiB fixture file, then answers 2 KiB of Markdown | `agent_end` of the 40th prompt | `session_early_turn_cpu_ms` (median of turns 2–6), `session_late_turn_cpu_ms` (median of 36–40), `session_end_rss_mib`, `session_peak_rss_mib` |

- **Pacing** only where it is the point: the long answer. An unpaced stream lands in a few frames and hides the per-frame cost. The other scripts stream as fast as Pi reads.
- **pty runs** take the prompt as Pi's initial message, `--no-session`, and end by quitting as a user would, so the exit status is still checked.
- **Per-turn CPU** is `/proc/<pid>/stat` utime + stime read at each prompt and its `agent_end`. Late against early turn is the growth; RSS at the end is the memory side.
- **`--perf-stat`** adds `<scenario>_minstr` for each, as for the existing metrics.
- **`--scenarios <list>`** picks which run; the default is all. Sizes are constants. `--big-write-kib 0` keeps skipping both writes. A full round on stock is roughly a minute; startup-only iteration is `--scenarios startup,interactive`.

## Steps

1. **Prototype by hand** on stock Pi and the default backend:
   - Pi submits an initial message in interactive mode; how a pty session ends cleanly (`/quit`, or Ctrl+D on an empty editor);
   - the end markers are reliably in the drawn text (Pi redraws changed lines only; p1's lesson: scan all text drawn, not the last frame);
   - RPC: the `prompt` command and the event that ends a turn in 1.3.0; a kept session file in the harness home, not the user's;
   - extensions' own model calls (an observer) get the closing text, as in `big_write`, and do not break the turn count.
2. **Implement** in `src/fake_provider.rs` (scripts, content, pacing) and `src/bench.rs` (runs, metrics, `--scenarios`).
3. **Tests:** unit tests for the generated content (exact sizes, valid UTF-8 at every delta boundary), the pacing, and the turn CPU arithmetic. `PIBAKE_E2E=1`: every scenario on the fixture with stock and a built executable, and each end check holds.
4. **Profile each scenario** on stock and on `build --profiling` of the default backend (p2's `perf record`), plus one `--cpu-prof` run of stock for JS-level names. Run `long_session` once on the live setup as well, with its extensions. Log the top self-time functions per scenario in `progress.md`.
5. **Rank.** From steps 4 and 6, replace the session items in the README's Candidates with a ranked list: scenario, cost, the Pi function, the likely fix (Pi-Bolt's or our own). That list is the input for the next round's plans. p8 stays the one session fix in this pass.
6. **Gate:** ask the owner for an idle machine. Record `bench --runs 10 --perf-stat --scenarios long_answer,tui_write,big_output,long_session --record p7_session` with stock and the default backend, both built from one lock.

## Completion

- [ ] The new metrics appear in `bench` output and recorded JSON for every command.
- [ ] Unit and E2E tests green; a scenario that never reaches its end fails.
- [ ] Per-scenario profiles logged in `progress.md`; the ranked list in the README.
- [ ] Recorded bench in `bench_results/`.
- [ ] `cargo fmt --check`, clippy and every test green, E2E included.

## Non-goals

- Any optimisation. p8 is the long-answer fix; the rest go to the next round, ranked.
- A screen model or a colour-accurate render check. Parity of what is drawn is p8's test, at the component.
- Typing latency and big pastes in the editor. They need a key-driven harness; Pi-Bolt's fixes for them stay a small candidate.
- The TUI over a many-turn transcript (see Finding: 0.19 ms per frame).
