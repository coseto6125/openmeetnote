# Vendored dependencies

One crate is vendored here because it carries local patches. It is
otherwise byte-identical to the crates.io release it was taken from, so
`diff` against the registry source shows exactly what changed and nothing else.

It lives in-tree rather than in a fork so that `git clone && cargo build`
works with no extra setup, and so the patch is reviewable in the same commit
as the code that depends on it.

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
