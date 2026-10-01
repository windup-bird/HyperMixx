# `charon-audio` 0.1.2, vendored with one functional patch

Hypermixx carries this copy because the published crate cannot run the model on a GPU, and the
missing piece is one enum variant. Everything else in here is upstream 0.1.2, byte for byte.

The patch is `charon-audio-hypermixx.patch` (apply with `patch -p1` against a fresh copy of the
crate). It is deliberately small and shaped like the change upstream would accept: their crate
already has a CoreML execution provider, so a CUDA one is the same idea.

## What the patch does

1. **`cuda` feature** — `cuda = ["ort-backend", "ort/cuda"]`, mirroring the existing `coreml` one.
2. **`ExecutionProvider::Cuda`** — a third variant beside `Cpu`/`CoreMl`, plus the session shape it
   needs (`build_session` now takes the provider instead of a `coreml: bool`, so the CPU path is
   untouched).
3. **`with_cuda`** registers `ort::ep::CUDA` **and sets `session.disable_cpu_ep_fallback=1`.** That
   setting is the point: without it, a CUDA session that cannot take the graph silently runs on the
   CPU at a ninth of the speed. A front-end must be able to tell "the GPU did the work" from "the
   GPU was asked for and ignored".
4. **`ConstantFolding` is kept enabled on the CUDA path.** This is the non-obvious one.
   `OnnxOptions::low_memory()` — which `ModelConfig::htdemucs()` inherits — disables that optimizer
   to cap RAM, and with it disabled **ONNX Runtime's CUDA execution provider claims no nodes at
   all**: the whole 1201-node graph lands on the CPU EP. Measured on `htdemucs.onnx`, a session built
   every other way is identical:

   | session | outcome |
   |---|---|
   | CUDA, `disabled_optimizers = []` | 214 ms / window, all nodes on CUDA |
   | CUDA, `memory_pattern = false` | 214 ms / window |
   | CUDA, `disabled_optimizers = ["ConstantFolding"]` | **no node claimed** (a hard failure under 3, a silent CPU run without it) |
   | CPU | 2055 ms / window |

   So upstream's comment on `ModelConfig::htdemucs` — *"the in-graph STFT export only runs on the CPU
   provider"* — is not accurate about the export: the export runs entirely on CUDA, and this preset
   is what stops it. The filter is applied only when the provider is CUDA, so the CPU path keeps
   upstream's memory behaviour exactly.

## What was removed when vendoring

The published crate ships its test suite (1.6 MB of audio fixtures), benchmarks and CLI/examples.
Only `Cargo.toml`, `src/` and the two licences are kept, so the `[[bin]]`, `[[example]]`, `[[bench]]`
and `[[test]]` target declarations are commented out and `src/bin/` is gone. Those deletions are in
the patch file too, marked `PATCH(hypermixx): dropped upstream … target`.

## Re-vendoring against a newer upstream

```bash
V=0.2.0                                  # the version to move to
D=$(find ~/.cargo/registry/src -maxdepth 2 -name "charon-audio-$V" | head -1)
rm -rf vendor/charon-audio && mkdir -p vendor/charon-audio
cp "$D/Cargo.toml" "$D/LICENSE-APACHE" "$D/LICENSE-MIT" vendor/charon-audio/
cp -r "$D/src" vendor/charon-audio/ && rm -rf vendor/charon-audio/src/bin
cd vendor/charon-audio && patch -p1 --forward < charon-audio-hypermixx.patch   # re-apply, fix rejects
```

Then update `crates/hypermixx-stems/Cargo.toml` (the `charon-audio` version) and re-run:

```bash
cargo test -p hypermixx-stems
cargo test -p hypermixx-stems --features cuda
cargo test --release -p hypermixx-stems --features cuda -- --ignored real_model --nocapture
```

If upstream ever grows a CUDA provider of its own, delete this directory and the `[patch.crates-io]`
entry in the workspace `Cargo.toml` — that is the whole cleanup.
