# WT video E2E harness

Drives a real browser through pairing and a streaming session, then asserts on
**what is on the screen** rather than on decoder counters.

## Why it asserts on pixels

On 2026-09-07 this suite passed all night on Linux Chrome while a Mac and a
phone showed black. It could not have caught that: it asserted on
`wt first glass age`, which the client logs when the *decoder* emits a frame.
A decoder can run perfectly while nothing is painted — that is exactly the iOS
Safari failure, where `drawImage` of a `VideoFrame` silently draws nothing.

So the assertions are now:

| Assertion | Catches |
|---|---|
| canvas has ≥ 8 distinct colours in some sample | a black / flat screen while decode "works" |
| the sampled canvas hash changes at least once | a frozen last frame, which decode counters run straight through |
| `--kill`: host returns to Idle after a hard kill | the session slot leaking when a client dies |

The harness computes the verdict itself and writes `result.json`;
`tools/remote-e2e.sh` reads that rather than re-deriving a verdict by grepping
log text. One place decides whether a run passed.

## Running

```bash
# 45s soak against the tailnet address
tools/remote-e2e.sh 45

# also hard-kill the browser mid-session and check the slot is freed
E2E_STATUS=http://127.0.0.1:47800/api/v1/status HOST_SSH=user@pc tools/remote-e2e.sh 45 gaming-pc.local kill --kill

# the harness on its own
node tools/e2e/run.js "<invite-url>" 60 --out /tmp/wte2e
```

Needs `playwright-core` on `NODE_PATH`, and a Chrome binary — Playwright's own,
or one named by `E2E_CHROME`.

## Known limits

- **It is still one browser on one OS.** It cannot see a Safari-only or
  iOS-only render failure; only a real device can. What it *can* now do is fail
  when pixels are absent, so the same class of bug is catchable wherever the
  harness runs.
- **`everChanged` assumes the host screen has some motion** (a clock, a cursor).
  On a genuinely frozen desktop this could report a false failure — read it
  alongside the screenshots in the output directory before believing it.
- **Running from a machine on the host's LAN proves nothing about internet
  reachability**: the LAN IP and the global IPv6 are both on-link. Use an
  off-net vantage point for that.
