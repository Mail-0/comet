# Keiki

Keiki is a desktop workspace for coding with cloud agents, built on Zeron's
Rust engine and zgx desktop app.

## Development

```sh
cargo check --workspace
cargo check --manifest-path apps/zgx/Cargo.toml
```

The desktop app lives in `apps/zgx`; `apps/zeron` provides the headless engine
and CLI. Internal crate and executable names retain the `zeron` prefix.

Local data and default worktrees live in `~/.keiki`. On first launch, existing
local-only `~/.zeron` or `~/.comet-native` data is migrated, preserving legacy
paths on Unix so existing Git worktrees keep working. Directories containing
account stores or historical uploads remain untouched. `ZERON_DATA_DIR` overrides the
data directory and `ZERON_WORKTREES_DIR` overrides the worktree root.

Keiki OAuth uses the OS keyring and `KEIKI_API_URL` (default
`https://onkeiki.com`). Workspaces stay local when signing in or switching
organizations. Automatic release checks require an explicit
`KEIKI_RELEASES_URL`; there is no Zeron release-service fallback.

Upstream: [Zeron](https://github.com/zeronsh/zeron). See [LICENSE](LICENSE).
