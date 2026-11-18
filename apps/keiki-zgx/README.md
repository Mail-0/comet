# Keiki Desktop Preview

This opt-in executable uses Zeron v2's ZGX/Luau desktop framework with Comet's
local workspace RPC and Keiki API client. It does not replace GPUI, run a second
engine, load plugins, or connect to Zeron services.

ZGX is pinned to the revision used by [Zeron v2](https://github.com/zeronsh/zeron/commit/b1064c1bd89aa3428f66042be4b197738208ef66).
The preview has its own Cargo workspace and lockfile so its renderer and
windowing patches do not affect the shipping build. ZGX is MIT-licensed.

On Linux, use the existing Comet system dependencies plus `libdbus-1-dev`.
Run these commands from the Comet repository root:

```sh
cargo build --locked --manifest-path apps/keiki-zgx/Cargo.toml
cargo build --locked -p zeron

# Terminal 1: run Comet with an isolated data directory and unused port.
ZERON_DATA_DIR="$(mktemp -d)" ZERON_IPC_PORT=27881 target/debug/zeron headless

# Terminal 2: point the preview at that daemon.
KEIKI_PREVIEW_IPC_PORT=27881 apps/keiki-zgx/target/debug/keiki-zgx
```

The preview can also open without a daemon. Its workspace watch is read-only.
Sign in with Keiki in your browser, then choose an organization and a hosted
agent. The preview shows the latest streamed reply; it does not store
credentials or a transcript. Use a test organization and agent: sending a
message starts a real turn. Set `KEIKI_API_URL` to test against a different
Keiki environment.
