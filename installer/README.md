# InPhase Host installer

`scripts/build-installer.ps1` drives the full chain:

```
build.ps1 -Release   ->  target\release\inphase-host.exe  +  web\dist\
package.ps1          ->  dist\InPhase\   (exe + a license-clean private
                          GStreamer 1.28.x runtime — only the plugins InPhase
                          loads, only the DLLs those import; MANIFEST.csv +
                          OPEN-SOURCE-COMPONENTS.txt + licenses\)
inphase.iss (ISCC)   ->  dist\InPhaseSetup.exe
```

The installer copies `dist\InPhase\` to `Program Files\InPhase`, adds the
per-user "start at sign-in" Run-key entry, and runs `inphase-host --trust-ca`
elevated so the bundled local CA is trusted on this PC. The host is a windowless
tray application: no launcher script, no hidden console. It self-manages its
Windows Firewall rules on every start.

**Not signed.** Pass `/DSignTool="<signtool cmd with $f>"` to ISCC (or set
`SIGNTOOL`) for a release build; unsigned builds compile with a warning.

GStreamer is pinned to 1.28.x for the shipping branch (ADR-002); bump only after
the compatibility suite passes on all three GPU vendors.
