# N1: Keep the provider connection across think time

## Finding

- **Every prompt after 4 s of idle opens a new connection.** Pi's dispatcher (`http-dispatcher.js`, `configureHttpDispatcher`: `EnvHttpProxyAgent` with `allowH2: false`, timeouts and its own client and pool factories) sets no `keepAliveTimeout`. undici then closes an idle socket after its default of 4 s, unless the server sends a `Keep-Alive: timeout=` hint. `api.anthropic.com` (Cloudflare) sends none: `Connection: keep-alive` only.
- **A human prompt nearly always comes after more than 4 s.** The user reads the answer and types. A tool call that runs longer than 4 s (a build, a test run) costs the next request a new connection too.
- **Measured:** Pi's undici 8.10.2 from the stage, built as Pi builds it, against `api.anthropic.com` with unauthenticated requests (401s). Connect times come from undici's `undici:client:beforeConnect` and `undici:client:connected` diagnostics channels (`progress.md` → Probe round):

  | Gap before the request | Default (4 s) | `keepAliveTimeout: 60000` |
  |---|---|---|
  | first request | new connection, 59 ms | new connection, 59 ms |
  | 0 s | reused | reused |
  | 5 s | new connection, 58–65 ms | reused |
  | 6 s | new connection, 55–58 ms | reused |

  curl agrees: DNS 1–5 ms, TCP 20–24 ms, TLS 25–40 ms (Cloudflare NRT, RTT ~20 ms).
- **Napkin floor: 0 ms** for a connection kept warm. The cost is two round trips (TCP plus TLS 1.3) per idle prompt, on the critical path before the model's first token. A warm turn costs Pi 6–8 ms by comparison (`progress.md` → Probe round).

## Change

Pass a `keepAliveTimeout` to Pi's `EnvHttpProxyAgent`. undici passes it on through Pi's `factory` and `clientFactory` to each origin's `Pool` and `Client` (checked: idle gaps of 5 and 6 s reuse the connection). A server's `Keep-Alive: timeout=` hint still wins when one is sent. Idle sockets are unref'd, so they don't hold the process open at exit.

The value must stay below the server's own idle timeout. Otherwise a request can be written onto a socket the server is closing, and it fails with `other side closed`. Pi and the SDKs retry that error, but after a back-off that costs more than the handshake saved. `api.anthropic.com` kept an idle connection open for more than 20 minutes (`progress.md`), so on the live setup's main provider, the server's timeout doesn't limit the value.

Where it lives is the owner's call (O7):

- a pass, default on;
- a pass, opt-in;
- an upstream PR to Pi, which stock Pi benefits from too.

It changes how long Pi holds a connection, but not what it sends. If it is a pass, it sits next to `lazy_http_dispatcher`, matched by exact text on the `EnvHttpProxyAgent` options. It applies whether that pass is on or off.

## Steps

1. **Probe the servers' idle timeouts:** an unauthenticated keep-alive request to each origin the live setup uses, then wait for the server to close (`target/n1/ka_probe.py`). `api.anthropic.com`: > 20 min, done. The OVH endpoints in `models.json` remain, if reachable. Choose the value: below the smallest timeout found, with a margin, and at most a few minutes.
2. **Confirm in Pi itself:** a `--require` hook logging connects (`target/n1/connlog.cjs`) on the built executable. Run a live interactive session with two prompts 10 s apart, owner-run since it uses the owner's credentials. Expect two connects today and one with the change.
3. **O7:** take the numbers to the owner.
4. **Implement** the pass (or the PR), with a check that the options object has the expected shape.
5. **Tests:**
   - a unit test of the rewrite;
   - an E2E test: a keep-alive variant of the fake provider (the scripts answer with `Connection: close` today), two RPC prompts 5 s apart, and one connection accepted. It fails without the change;
   - the p5 proxy and idle-timeout E2E tests still green;
   - `verify` same.
6. **Measure:** no `bench` metric moves; the fake provider closes every connection. The result is step 2's connect log and a timed pair of live prompts, both by hand.

## Completion

- [ ] A prompt after 5–60 s of idle reuses the connection, by E2E test (done) and on the live setup (owner).
- [ ] No `other side closed` errors over a day of owner use.
- [x] `cargo fmt --check`, clippy and every test green, E2E included.

## Non-goals

- Preconnecting before the first request, which needs a request or undici internals (README Candidates).
- HTTP/2 (`allowH2: false` is Pi's choice).
- Proxies' own keep-alive (`ProxyAgent`): stock behaviour.
