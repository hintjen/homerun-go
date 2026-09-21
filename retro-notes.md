- 2026-09-21 [phase 1] The biggest finding of the run, and I only have it because subagent E
  read prior art instead of vendor docs: **Pterodactyl's `"done": "Server startup complete"` is
  not matched on the game's stdout.** I fetched `wrapper.js` myself to be sure. It removes its
  listeners on `gameProcess.stdout` and `gameProcess.stderr` the moment the RCON websocket
  opens - which happens *before* map generation finishes - and from then on prints only RCON
  messages. So the one ready marker every egg encodes is observed on the RCON stream. Our
  runner matches `ready.marker` against the child's stdout/stderr. Nobody in any prior art has
  ever confirmed that line reaching stdout on Windows. -> the ready hypothesis is weaker than
  the brief or the contracts file assume, and this is the single thing a probe most needs to
  settle.   (routed: open)
- 2026-09-21 [phase 1] Related and equally load-bearing: **RustDedicated does not take console
  commands on stdin, on either platform.** Verified two ways - `wrapper.js` can run no command
  at all before RCON is up (it falls back to SIGTERM), and LinuxGSM issue #4720 (open, Dec 2024)
  reports `stopmode=quit` failing precisely "because LGSM tries to send it to the server console
  instead of via RCON", with the signal path "forgetting teams, because the server doesn't get a
  chance to generate a full save". `console.via` must be `rcon`; a stdin descriptor would be
  wrong.   (routed: open)
- 2026-09-21 [phase 1] Every Windows-capable manager in prior art (AMP, RustSM, RustServerManager)
  uses **no log-line readiness at all** - they poll until RCON connects and authenticates, or
  they only check the process is alive. The engine offers exactly one readiness mechanism, a
  marker on the output. If the Windows binary turns out not to print the marker to stdout, the
  descriptor cannot express the readiness signal the whole ecosystem actually uses. That is a
  schema/engine gap, not a Rust quirk: "readiness = the console port answers".   (routed: open)
- 2026-09-21 [phase 2] `licence` in schema v0 is a single `{name, url}`. Rust needs **three**
  documents accepted (Steam Subscriber Agreement, Facepunch ToS, Facepunch Community Server and
  Hosting Guidelines). I can name them all in one string, but then the URL is a lie for two of
  them and the API stores one URL as "the terms as they were at that moment". -> the schema
  needs `licence` to be a list. First real YELLOW schema gap of the pipeline.   (routed: open)
