# runar-muninn

The single Rust crate that ships the `runar` binary — Huginn (scout)
+ Muninn (memory) + Curator (Q&A) in one static executable.

For features, install instructions, CLI reference, MCP tool catalogue,
and configuration, see the [workspace root README](../../README.md).

## Build

```bash
# From workspace root
cargo build --release -p runar-muninn
# Binary lands at ../../target/release/runar
```

## Test + lint

```bash
cargo test --release -p runar-muninn
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## Optional features

| Feature | Default | Effect |
|---|---|---|
| `local-embeddings` | off | Bundles fastembed + ONNX runtime for `RUNAR_EMBEDDINGS=local` (all-MiniLM-L6-v2, 384-dim). Adds ~70 MB to the binary; release CI flips it on for the shipped artefact. |

```bash
cargo build --release -p runar-muninn --features local-embeddings
```

## License

MIT. See [`LICENSE`](../../LICENSE) at the workspace root.
