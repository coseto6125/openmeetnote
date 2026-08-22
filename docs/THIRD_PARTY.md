# Third-party components

OpenMeetNote itself is [MIT](../LICENSE). It ships and links the following,
each under its own license.

## Bundled at runtime (you download these separately)

| Component | License | Role |
|---|---|---|
| [whisper.cpp](https://github.com/ggml-org/whisper.cpp) | MIT | Final-transcript inference |
| [sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) | Apache-2.0 | Live transcript, VAD, punctuation, speaker embedding |
| [ONNX Runtime](https://github.com/microsoft/onnxruntime) | MIT | Backend for sherpa-onnx 與 `ort`（語者分割模型直接推論） |

Speech models carry their own terms, set by whoever published them. They are
not distributed with this project.

## Vendored in this repository

Both live under `src-tauri/vendor/` with local patches. See
[`src-tauri/vendor/README.md`](../src-tauri/vendor/README.md) for the exact
diff and the reason for each change.

| Component | License | Patch |
|---|---|---|
| [whisper-rs-sys](https://github.com/tazz4843/whisper-rs) 0.15.0 | MIT | 11 lines in `build.rs`: enable x86 SIMD when cross-compiling |
| [sherpa-rs](https://github.com/thewh1teagle/sherpa-rs) 0.6.8 | MIT | Null dereference, a leak, and exposing the VAD `reset` the C API already had |

## Build-time and linked crates

The full dependency tree with licenses is reproducible from the lock file:

```bash
cargo install cargo-license
cargo license --manifest-path src-tauri/Cargo.toml
```

## Design references

The project referred to as "Minutes" in [BLUEPRINT.md §19](../BLUEPRINT.md)
was read as an architectural reference for the system-audio interface, macOS
capture, summary providers and secret storage. **No code was copied.** Had any
been, its license and copyright notice would appear in this file and in the
source that carried it.
