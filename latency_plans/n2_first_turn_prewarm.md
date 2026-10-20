# N2: Warm the first turn while the user types

## Finding

- **The first prompt of a session takes 10× longer than later ones to reach the provider.** RPC, `long_session` through an RPC tap (`target/n1/rpctap.py`), fake provider on localhost, time from writing the prompt to the first `message_update`. Turn 1: 64 and 91 ms (two runs). Turns 2–40: 6–8 ms, with up to 720 KiB of context. The recorded p4 bench agrees from the other side: a 1 KiB print turn costs 106 ms on top of RPC startup on `sea_codecache`, and 67 ms on `sea_trained`.
- **Where turn 1 goes** (`--cpu-prof`, the first 150 ms of the agent loop, `progress.md` → Probe round):
  - the provider SDK's module evaluation (`openai-completions.js` → `openai/client.mjs`), 24 ms. Of that, 16 ms is npm undici and Pi's dispatcher, which p5 deferred to the first use: the SDK's auth module reads `fetch` at load. On the live setup the SDK is `@anthropic-ai/sdk` (`anthropic-messages`);
  - the first `declareToolChanges`, 9 ms;
  - lazy compilation of the request path, spread across the stack. `sea_trained` caches it, so its turn 1 is 40 ms shorter.
- **The user's typing is idle time.** In interactive mode, after the ready frame, nothing runs until the prompt. Typing a prompt takes seconds; the work above takes ~50–90 ms. RPC clients wait too, between `get_state` and their first prompt. Online interactive startup already materialises the dispatcher after ready (p5: `checkForNewPiVersion`). Offline, and in RPC, nothing does.
- **Napkin floor:** the first turn should cost what a warm turn costs, ~7 ms.

## Change

After ready, run the first turn's set-up in idle time, as a chain of macrotasks, so a keystroke waits for one link at most:

1. evaluate the current model's API module through pi-ai's own lazy loader (the one `streamSimple` uses), so the SDK's module init runs;
2. read `globalThis.fetch`, which materialises p5's deferred dispatcher as a request would.

Hooks: interactive mode after `init()`'s ready frame; RPC mode after its ready state. Print mode has no idle time and stays as it is.

Nothing a request could observe changes: the same modules are evaluated and the same dispatcher is created, only sooner. On `model_select`, the new model's module is warmed the same way.

Where it lives is the owner's call (O8): a pass, default on or opt-in, or an upstream PR.

## Steps

1. **Probe by hand:** patch the bundle's interactive and RPC start with both links, and measure:
   - RPC turn 1 through `rpctap.py`, with a 1 s pause after `get_state`;
   - interactive: from the prompt's Enter to the provider's first byte, with a pty tap on an offline harness;
   - each link's duration, as the longest a keystroke could wait.

   Expect turn 1 to fall from 64–91 ms to 20–40 ms. If the module and dispatcher do not account for most of the gap, look into what does (`declareToolChanges`, compilation) before going on.
2. **Map side effects:** what the provider modules and the dispatcher do when evaluated (globals, timers, env reads), and whether any extension depends on them being late. p6's review is the template.
3. **O8:** take the numbers and the map to the owner.
4. **Implement** the pass (or PR).
5. **Tests:**
   - E2E: RPC with a pause before the first prompt; the request the fake provider records is byte-for-byte stock's;
   - the p5 proxy and idle-timeout tests with the warm-up having run first;
   - `verify` same.
6. **Measure:** add a `first_turn_ms` metric to `bench` (RPC: prompt to the first `message_update`, after a fixed pause). It needs a bench change, kept small. Run `bench --runs 10 --perf-stat` on `sea_codecache` and `sea_trained`. Startup and ready must not move: the warm-up runs after them.
7. **Gate:** an idle machine (owner). Record `bench --runs 20 --perf-stat --record n2_prewarm`.

## Completion

- [ ] `first_turn_ms` falls by at least half on `sea_codecache`; startup, ready and every other scenario flat.
- [ ] No warm-up link longer than ~25 ms on the default backend.
- [ ] `cargo fmt --check`, clippy and every test green, E2E included.

## Non-goals

- Print mode: its turn starts at once.
- Opening the connection early (N1's non-goal; README Candidates).
- Warming the agent's per-turn state (tool declarations, context): that changes the agent's own state, not code loading.
