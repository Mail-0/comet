# Keiki Desktop

Native desktop client for creating, managing, and communicating with
[Keiki](https://app.keiki.ai) agents.

This milestone establishes the macOS GPUI shell and the first Keiki-native data
contracts. Agent synchronization, authentication, creation, conversations, live
tests, and traces will be connected incrementally.

## Develop

Requirements:

- macOS 12 or newer
- the stable Rust toolchain from `rust-toolchain.toml`

```bash
cargo run -p keiki
```

The client uses `https://app.keiki.ai` by default. Override it for local
development with `KEIKI_API_URL`; override settings storage with
`KEIKI_DATA_DIR`.

## Verify

```bash
cargo fmt --check
cargo test --workspace
cargo check --workspace
```

## Package for macOS

```bash
scripts/package-macos.sh
```

Artifacts are written to `target/package`.

## Upstream

Keiki Desktop is a fork of [zeronsh/comet](https://github.com/zeronsh/comet).
The fork preserves comet's MIT license, Git history, GPUI shell foundations,
theme system, syntax highlighting, and third-party notices while replacing its
local coding-agent runtime with the Keiki service.

Licensed under the [MIT License](LICENSE).
