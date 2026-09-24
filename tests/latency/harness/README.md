# Frametrace latency / stress harness

Drives a real Chrome InPhase client over the DevTools protocol, pairs it, streams
for N seconds, and harvests the per-frame pipeline trace (host stamps on the
`control` channel + `requestVideoFrameCallback` on the client, joined on RTP
timestamp). Optionally runs Unigine Heaven alongside as a GPU load.

Results and the write-up from the first run: `results-2026-09-01/`, `FINDINGS.md`.

## Pieces

| file | where it runs | what it does |
|---|---|---|
| `cdp-probe.mjs` | dev box (needs `ws`) | attach to a Chrome target, navigate to `/?play`, pair, collect `[InPhase frame]` FrameStats + `getStats` telemetry, aggregate, write `<label>.{summary,raw}.json` |
| `hostagg.mjs` | dev box | aggregate the host `inphase_host::frametrace` summary lines |
| `runcdp.sh` | dev box | reset host session → `cdp-probe` → pull + aggregate the host.log slice |
| `pc-client-start.ps1` | gaming PC (console session) | launch Chrome with `--remote-debugging-port=9222` |
| `pc-heaven-start.ps1` | gaming PC | launch Unigine Heaven windowed as GPU load |
| `pc-gpu-poll.ps1` | gaming PC | `nvidia-smi` sampler → CSV |
| `pc-reset-session.ps1` | gaming PC | POST `admin/disconnect` so the next client isn't rejected (ADR-007) |
| `pc-hostlog-slice.ps1` | gaming PC | write the last N frametrace lines to `slice.txt`, unwrapped |
| `pc-stress-stop.ps1` / `pc-teardown.ps1` | gaming PC | kill Heaven + the CDP Chrome |
| `pc-_action.ps1` | gaming PC | runs `stress\action.txt` — target of a one-shot `InPhaseStress` interactive task, the only way to launch GUI processes into console session 1 over SSH |

## Run

On the gaming PC (once): create an interactive scheduled task that runs
`pc-_action.ps1`, and use it to launch the browser / Heaven into the desktop
session:

```
schtasks /create /tn InPhaseStress /tr "powershell -File C:\Users\YOUR_USER\stress\_action.ps1" /sc once /st 00:00 /it /ru YOUR_USER /f
# write the command into action.txt, then:  schtasks /run /tn InPhaseStress
```

From the dev box:

```
ssh -N -L 19222:127.0.0.1:9222 YOUR_USER@<pc>          # tunnel to the PC's Chrome CDP
echo <PIN> > .pin
bash runcdp.sh pcloop-idle-buf120 19222 http://127.0.0.1:47800 120 \
  '{"width":2560,"height":1440,"fps":60,"maxBitrateKbps":40000,"bufferMs":120,"preset":"low_latency"}'
```

`cdp-probe.mjs` also works against a local Chromium — point `--port` at its
`--remote-debugging-port` and `--origin` at the host's LAN IP. Headless Chrome
without a GPU software-decodes and will choke at 1440p60; only its `net` numbers
are then trustworthy.

## Gotchas learned

- `http://127.0.0.1:47800/` serves the **host dashboard**; append `?play` for the
  player (`web/src/main.ts`).
- Never CDP-close the last page target — headed Chrome exits.
- A killed `cdp-probe` leaves the host stuck `Playing`; `runcdp.sh` calls
  `pc-reset-session.ps1` first.
- PowerShell over SSH mangles quotes — every PC-side step is a `.ps1` run with
  `powershell -File`.
