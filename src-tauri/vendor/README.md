# Vendored dependencies

Two crates are vendored here because they carry local patches. Both are
otherwise byte-identical to the crates.io release they were taken from, so
`diff` against the registry source shows exactly what changed and nothing else.

They live in-tree rather than in forks so that `git clone && cargo build`
works with no extra setup, and so the patch is reviewable in the same commit
as the code that depends on it.

## `whisper-rs-sys` 0.15.0 — 11 lines in `build.rs`

Cross-compilation does not enable x86 SIMD: `GGML_NATIVE` probes the *build
host*, not the target. Building the Windows binary from Linux therefore fell
back to a scalar path and ran **nine times slower** (RTF 3.96 against 0.45
for a native build).

The patch sets the four instruction-set defines explicitly for `x86_64`
targets. Everything else, including the bundled `whisper.cpp` sources, is
upstream.

```
$ diff -u ~/.cargo/registry/src/*/whisper-rs-sys-0.15.0/build.rs whisper-rs-sys/build.rs
```

Upstream: <https://github.com/tazz4843/whisper-rs> (MIT)
Bundled `whisper.cpp`: <https://github.com/ggml-org/whisper.cpp> (MIT)

## `sherpa-rs` 0.6.8 — three small fixes in two files

1. **`diarize.rs`: null dereference.** The C API returns `NULL` when
   diarization fails; upstream passes it straight to `GetNumSegments`. On
   Windows that is an access violation (`0xc0000005`).
2. **`diarize.rs`: leak on the "no segments" path.** Upstream `bail!`s before
   the two release calls, so every batch that produces no speakers leaks the
   result. It corrupts the heap after enough repetitions — which shows up as
   "crashes after running for a while", not as a reproducible crash.
3. **`silero_vad.rs`: expose `reset()`.** The C API has
   `SherpaOnnxVoiceActivityDetectorReset` but the Rust wrapper only binds
   `Clear`, which drains the segment queue without resetting the model's
   recurrent state. Judging independent buffers with one detector then leaks
   the previous buffer's speech into the next verdict — measured as noise
   scored at 1100 ms of speech directly after a speech buffer, and 0 ms when
   judged on its own.

```
$ diff -ru ~/.cargo/registry/src/*/sherpa-rs-0.6.8/src sherpa-rs/src
```

Upstream: <https://github.com/thewh1teagle/sherpa-rs> (MIT)
