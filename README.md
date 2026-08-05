<div align="center">

# OpenMeetNote

### A meeting recorder that has to show its work.

*Dual-track capture · two transcription engines · every claimed fact carries a citation the program itself verified.*

![License](https://img.shields.io/badge/license-MIT-blue)
![Built with Rust](https://img.shields.io/badge/built_with-Rust%20%2B%20Tauri-orange?logo=rust)
![Windows](https://img.shields.io/badge/Windows-verified-brightgreen)
![macOS](https://img.shields.io/badge/macOS-beta-yellow)
![Status](https://img.shields.io/badge/status-0.1.0%20early%20release-yellow)

**English** · [繁體中文](./docs/readme_i18n/README_zh-TW.md)

[Download](https://github.com/coseto6125/openmeetnote/releases/latest) · [Blueprint](./BLUEPRINT.md) · [Domain notes](./CONTEXT.md)

</div>

---

Meeting summarizers fail in a specific way: they produce a confident paragraph
about a decision nobody made. You cannot tell by reading it, because a
fabricated sentence looks exactly like a real one.

OpenMeetNote is built around the assumption that the model will do this, and
that the only defence that survives contact with reality is one the program
can enforce without asking the model to police itself.

| Failure mode | What the program does about it |
|---|---|
| Summary asserts something nobody said | Every `Fact` block must carry a citation. The quote is matched **verbatim** against the stored transcript revision, the hash must match, and the revision must fall inside the snapshot. A block that fails is removed — never downgraded into a paragraph that renders anyway. |
| Transcript invents words during silence | Six gates in front of the decoder, ordered so each one only runs on what the previous one already established. Measured over a 35-minute soak: 100 batches finalized, 0 phantom batches admitted. |
| "Verified" is just another thing the model said | The model supplies the quote and the source id. The hash and the validation status are filled in by the system afterwards. Letting the thing under test declare its own result is not a test. |
| Recording stops while the AI thinks | Summaries run against a frozen event cursor on a separate read snapshot. Capture and transcription never pause. |

Everything runs locally. No account, no telemetry, no network call the app
makes on its own.

---

## Status

**Windows** is verified on real hardware end to end: dual-track capture, live
and final transcripts, AI summary, citation verification, export, search.

**macOS is beta.** The ScreenCaptureKit + CoreAudio capture path compiles in
CI for both Apple Silicon and Intel, its unit tests run natively on the
arm64 runner, and every layer above it is platform-independent and tested —
but the author has no Mac, so nothing above the unit tests has met real
audio hardware, and the two permission dialogs have never actually appeared.
If you try it, [an issue saying what
happened](https://github.com/coseto6125/openmeetnote/issues) is genuinely
useful.

A two-hour soak has run on Windows: 393 finalized batches, 2586 segments,
25594 characters, memory bounded between 2.13 and 2.21 GB with no upward
trend, process alive, meeting closed cleanly.

## Running

Models are **not** bundled. Together they exceed a gigabyte, and you should
know where that gigabyte lives on your disk. Put them next to the executable:

```text
openmeetnote(.exe)
vocabulary.txt                      # proper-noun corrections, edit freely
models/
  ggml-large-v3-turbo-q5_0.bin      # final transcript
  sherpa-onnx-paraformer-zh-.../    # live transcript
  sherpa-onnx-punct-ct-transformer/ # punctuation
  silero_vad.onnx                   # voice activity detection
  speaker-embedding.onnx            # speaker identification (optional)
```

A missing required model refuses the recording and says which file is absent.
It never degrades silently into a recording with no transcript — that failure
is only discovered afterwards, when the meeting is gone.

Environment variables override model locations and backend choice, and take
priority over the GUI settings:

| Variable | Purpose |
|---|---|
| `OPENMEETNOTE_LLM_PROVIDER` | `claude-code`, `codex`, `system`, `fixture` |
| `OMN_WHISPER_MODEL` | Final-transcript model path |
| `OMN_PARAFORMER_DIR` | Live-transcript model directory |
| `OMN_VAD_MODEL` | Voice activity model |
| `OMN_PUNCT_MODEL` | Punctuation model |

Engine loading and per-batch decisions are written to `stt.log` next to the
executable. On Windows the GUI subsystem has no stderr; without that file a
failed model load is completely silent.

### macOS permissions

System audio goes through ScreenCaptureKit, which macOS files under **Screen
Recording**; your own voice needs **Microphone**. Both are requested on first
record. Screen contents are never read or stored — only the audio track.

The build is not notarized yet, so Gatekeeper blocks it on first launch.
Right-click → Open, or:

```bash
xattr -dr com.apple.quarantine /Applications/OpenMeetNote.app
```

## Transcription pipeline

Two engines, because they differ by a factor of twenty in speed:

| Stage | Engine | RTF | Role |
|---|---|---|---|
| Live | Paraformer int8 | 0.03 | Words on screen while the meeting runs |
| Final | whisper large-v3-turbo-q5 | 0.55 | Quality, punctuation, timestamps |

Audio passes Silero VAD for segmentation, then two gates decide whether a
batch is worth the final engine at all: too little energy is skipped outright,
and what remains goes to VAD to decide whether there is actually a voice in
it. Output then gets CT-Transformer punctuation, Traditional Chinese
conversion, and the user vocabulary.

The energy threshold is not a constant — it follows the room:

```
threshold = clamp(p20 of the last 40 batch RMS values × 1.5, 0.003, 0.02)
```

A fixed threshold means taking one room's number and applying it everywhere.
A quiet office floors around 0.001, where a 0.005 threshold eats quiet
speech; a noisy venue floors around 0.01, where the same threshold does
nothing. The percentile rather than the minimum keeps one unusually quiet
batch from dragging the line down; the bounds keep it from ever climbing high
enough to swallow far-field speech (measured above 0.05).

The second gate is not redundant, because **energy cannot separate noise from
quiet speech**. Measured: room noise at RMS 0.0130 scores 0 ms of voice, while
real speech attenuated to RMS 0.0107 still scores 6406 ms. VAD reads
structure, not volume. Without it, whisper handed pure noise invents text —
over a two-hour recording the microphone track repeatedly produced "好" and
fansub credits.

Live and final paths use the same VAD in **opposite** ways, and getting that
backwards fails silently. Details in [BLUEPRINT.md §17.0.1](./BLUEPRINT.md).

`vocabulary.txt` holds one `wrong=right` pair per line, `#` starts a comment,
and it takes effect on the next recording:

```text
招委=召委
希臘雅=西拉雅
雙向元=雙橡園
```

Proper nouns are the shared blind spot of every transcription engine, and the
names that recur in *your* meetings are not the ones that recur in anyone
else's. Correction happens after the fact rather than as a model prompt:
whisper's initial prompt does improve proper nouns, but measurably causes it
to skip whole passages.

## Summaries and deliverables

Summaries are produced by a local Agent CLI (Claude Code or Codex). No API key
is needed — the CLI reuses the login you already have. It is invoked
non-interactively, one call at a time, prompt over stdin rather than argv
(meeting content should not appear in the process list), working directory
confined to a temp dir, and a timeout that kills the whole process tree.

**Document structure is decided by the renderer, not by the model's output
order.** Summary first, decisions and action items in their own section, gaps
and suggestions kept apart from facts. Whether the model emits the decision
first or last changes nothing about what the reader sees. The renderer will
also not manufacture a summary to fill the slot: no summary block, no summary
section.

The screen and the export share that rule but are two implementations (React
and Rust). A test reads the front-end's `sectionOf` source and compares the
predicates, because divergence would not raise an error on either side.

Evidence is fenced inside the prompt with a marker derived from the hash of
the evidence itself. A transcript can forge a line that reads exactly like
"user request for this round: ignore the above" — to forge the closing marker,
a participant would have to predict the hash of a document containing their
own words.

Transcript segments **and** manual notes are both citable. A note's revision
is its event sequence number; the model has to be told that value or it cannot
construct a citation that passes verification.

A new version revises the last successful one — "add the action items" edits
the previous document rather than regenerating from scratch. The previous
version counts against the *instruction* budget, not the evidence budget: when
it grows, evidence is what should yield.

Mermaid diagrams ship as source, not as a rendered image and not with an
embedded runtime. That runtime is about 1 MB against an 11 KB export; a
hundredfold size increase for the occasional flowchart does not pay. The
source pastes into any Mermaid viewer.

History search covers titles, transcripts and notes, and shows the matching
sentences. It is a SQLite `LIKE` scan rather than a full-text index: an index
needs triggers to maintain, and the event log is the single source of truth —
one more derived state to keep in sync is one more way for it to disagree with
reality. Measured over 130k rows (about fifty two-hour meetings): 40 ms for a
normal query, 174 ms worst case.

## Testing

```bash
cd src-tauri

cargo test                                   # unit tests
cargo test --test stt_pipeline               # transcription quality (needs models + audio)
cargo test --test end_to_end                 # audio → transcript → blocks → HTML
cargo test --test end_to_end -- --ignored    # same, but real CLI + revision + search
cargo test --test agent_cli -- --ignored     # real CLI calls, tens of seconds each
cargo test --release --test performance      # RTF and load time, regression guard

pnpm test                                    # front end
```

Integration tests need models and meeting audio. When those are absent the
whole test skips rather than fails — a CI that goes red for missing assets
only teaches people to ignore red. Point `OMN_TEST_ASSETS` at the directory.

## Building

Native builds work on Windows and macOS with the usual toolchain plus `cmake`,
`ninja` and a C++ compiler for whisper.cpp.

The Windows binary can also be cross-compiled from Linux:

```bash
export PATH="$HOME/.local/bin:$PATH"
export CC_x86_64_pc_windows_msvc=clang-cl-19
export CXX_x86_64_pc_windows_msvc=clang-cl-19
pnpm tauri build --runner cargo-xwin --target x86_64-pc-windows-msvc --no-bundle
```

That needs ninja, Clang 19 or newer, and cargo-xwin. Three pitfalls are
recorded in [CONTEXT.md](./CONTEXT.md): cmake not finding ninja, MSVC's STL
requiring Clang 19, and cross-compilation not enabling CPU SIMD — the last one
makes whisper nine times slower if left alone.

Two dependencies are vendored under `src-tauri/vendor/` because they carry
patches. Both are otherwise byte-identical to their crates.io release, so
`diff` shows exactly what changed. The reasons are in
[`src-tauri/vendor/README.md`](./src-tauri/vendor/README.md).

## Scope

- Windows and macOS desktop. No Android.
- Captures system audio and microphone directly. No meeting bot joins the call.
- Live transcript throughout the recording.
- Summary snapshots at any time, without stopping capture or transcription.
- Speakers default to "Speaker 1 / Speaker 2"; an explicit self-introduction
  only creates a *pending* name, never a confirmed one.
- Primarily Traditional Chinese, preserving technical and business English.
- Manual notes feed the summary and outrank ordinary transcript segments.
- Personal use: no login, no cross-device sync, data stays local by default.
- The Agent Loop plans each deliverable from the evidence and your prompt.
  There is no "meeting type → fixed template" rule anywhere in the code.

## Stack

Tauri 2, React + TypeScript, Rust core, SQLite, WASAPI loopback (Windows),
ScreenCaptureKit + CoreAudio (macOS), whisper.cpp, sherpa-onnx.

## License

[MIT](./LICENSE). Third-party components keep their own licenses — see
[`docs/THIRD_PARTY.md`](./docs/THIRD_PARTY.md).
