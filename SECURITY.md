# Security Policy

## Reporting a vulnerability

Use [GitHub private vulnerability reporting](https://github.com/coseto6125/openmeetnote/security/advisories/new)
(Security → Report a vulnerability). Reports are acknowledged on a best-effort
basis — this is a single-maintainer project; there is no SLA. Please do not
open public issues for exploitable bugs.

## Supported versions

Only the latest released `0.x` minor receives fixes. There are no backports.

## What OpenMeetNote touches on your machine

Threat-model transparency — everything the app reads or writes, so you can
audit the claims against the source:

| Surface | Path | When |
|---|---|---|
| Meeting database | `<app data>/openmeetnote.sqlite3` | Every meeting; event log is the source of truth |
| Exported documents | `<app data>/exports/*.html` | Only when you press Export |
| Engine log | `stt.log` next to the executable | Always; model loading and per-batch decisions |
| Speech models | Wherever you put them | Read-only, at record time |
| Microphone | OS capture API | Only between Start and Stop |
| System audio | WASAPI loopback (Windows) / ScreenCaptureKit (macOS) | Same |
| Agent CLI subprocess | `claude` / `codex` on your `PATH` | Only when you create a summary |

There is no telemetry, no account, and no network call the app makes on its
own. The only outbound traffic is whatever the Agent CLI you configured makes
when you ask for a summary.

## API keys

Keys live in the OS credential store (Windows Credential Manager, macOS
Keychain) or in environment variables. They are never written to SQLite, to a
config file, to the transcript, or to the log. `Secret` implements neither
`Debug` nor `Display` nor `Serialize`, and a test asserts the serialized
provider settings contain no key material.

The Agent CLI backends need no key at all — they reuse the login you already
did in that CLI.

## Untrusted content

Transcripts and attachments are treated as untrusted throughout:

- The HTML renderer takes structured blocks, never HTML strings. Text, URLs
  and attributes are escaped; only `http(s)` and in-document anchors survive
  the URL allowlist.
- Evidence sent to the model is fenced with a marker derived from the hash of
  the evidence itself, so a meeting participant cannot forge the boundary by
  speaking it — they would have to predict the hash of a document containing
  their own words.
- Every `Fact` citation is verified in code against the stored transcript
  revision: quote must be present verbatim, hash must match, revision must
  fall inside the snapshot. A block that fails is removed, never downgraded
  into a paragraph that renders anyway.

This is defence in depth, not proof. The citation check is the part that is
actually enforceable; the fence reduces the odds on a probabilistic surface.
