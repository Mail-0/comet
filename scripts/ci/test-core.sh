#!/usr/bin/env bash
# Linux Rust unit and integration tests, built together for nextest.
# Copilot's HTTP fixtures and Mock need no installed agent CLI.
# Extra arguments are passed to nextest (the nightly job adds `--release`).
# NEXTEST_PROFILE=ci (pull requests and main) leaves out the slow and soak tests
# that scripts/ci/nextest.toml lists; the nightly runs use the default profile.
set -euo pipefail
cd "$(dirname "$0")/../.."

# Test binaries that run nothing on Linux are left out: they would still be
# compiled and linked. Windows-only files (#![cfg(windows)]), and files whose
# every test is #[ignore]d because it needs a live service or a real CLI.
skip=" windows_native "
tests=()
for f in crates/harness/tests/*.rs crates/preview/tests/*.rs crates/doc/tests/*.rs; do
  name="$(basename "$f" .rs)"
  [[ "$skip" == *" $name "* ]] || tests+=(--test "$name")
done
# `misc` (harness, engine and preview): the small suites, one binary each.
# The orb geometry tests live in zeron-orb's `geometry`. `restart_hook`
# stays a binary of its own: the hook it checks is process-wide.
for t in misc geometry session_publication restart_resume side_chats message_queue device_routing device_room plugin_services restart_hook; do
  tests+=(--test "$t")
done

exec cargo nextest run --config-file scripts/ci/nextest.toml --locked --no-fail-fast \
  -p zeron-harness -p zeron-engine -p zeron-sync -p zeron-update -p zeron-doc -p zeron-preview \
  -p zeron-mcp -p zeron-proto -p zeron-rpc -p zeron-orb -p zeron-voice-session -p zeron-voice-media \
  -p zeron-cli -p zeron-launch -p zeron-notify \
  --lib --bins "${tests[@]}" \
  -E 'not ((package(zeron-proto) & not test(voice)) | (package(zeron-rpc) & kind(lib)))' "$@"
