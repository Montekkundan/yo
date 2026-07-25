#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"
real_home="${HOME:-}"

echo "==> Formatting"
cargo fmt --check

echo "==> Clippy"
cargo clippy --all-targets --all-features -- -D warnings

echo "==> Tests"
cargo test --all

echo "==> Local debug binary"
cargo build
yo_bin="$repo_root/target/debug/yo"
test_root="$(mktemp -d "${TMPDIR:-/tmp}/yo-local-test.XXXXXX")"
trap 'rm -rf "$test_root"' EXIT
mkdir -p "$test_root/home" "$test_root/config"

export HOME="$test_root/home"
export XDG_CONFIG_HOME="$test_root/config"
export YO_SESSION_ID="yo-local-smoke"

"$yo_bin" --version
"$yo_bin" current
"$yo_bin" permissions safe
"$yo_bin" permissions always-ask
"$yo_bin" permissions full-access --yes
"$yo_bin" permissions safe
"$yo_bin" eval --list
command_output="$("$yo_bin" run -- printf 'local-binary-ok\n')"
grep -q "local-binary-ok" <<<"$command_output"

if command -v zsh >/dev/null 2>&1; then
    "$yo_bin" init zsh | zsh -n
fi
if command -v bash >/dev/null 2>&1; then
    "$yo_bin" init bash | bash -n
fi
if command -v fish >/dev/null 2>&1; then
    "$yo_bin" init fish | fish -n
fi

if [[ "${YO_LIVE_TEST:-0}" == "1" ]]; then
    test_provider="${YO_TEST_PROVIDER:-}"
    if [[ -z "$test_provider" ]]; then
        if [[ -n "${AI_GATEWAY_API_KEY:-}" || -n "${VERCEL_OIDC_TOKEN:-}" ]]; then
            test_provider="vercel"
        elif [[ -n "${LLM_GATEWAY_API_KEY:-}" || -n "${LLMGATEWAY_API_KEY:-}" ]]; then
            test_provider="llm-gateway"
        elif [[ -n "${OPENROUTER_API_KEY:-}" ]]; then
            test_provider="open-router"
        else
            echo "YO_LIVE_TEST=1 requires a supported gateway credential" >&2
            exit 1
        fi
    fi
    echo "==> Live isolated Gateway setup ($test_provider)"
    "$yo_bin" setup --provider "$test_provider"
    "$yo_bin" current
    "$yo_bin" ask --private "Reply with exactly: yo-live-ok"

    echo "==> Live command-tool workflow evals"
    HOME="$real_home" "$yo_bin" eval terminal --verbose

    echo "==> Live cross-session automatic memory"
    memory_token="yo-retain-$(date +%s)-$$"
    YO_SESSION_ID="yo-live-memory-write" \
        "$yo_bin" ask "Remember across future terminal sessions that my live retention token is $memory_token."
    memory_list="$(YO_SESSION_ID="yo-live-memory-check" "$yo_bin" memory list)"
    grep -q "$memory_token" <<<"$memory_list"
    recalled="$(YO_SESSION_ID="yo-live-memory-recall" \
        "$yo_bin" ask "What is my live retention token? Reply with the token only.")"
    grep -q "$memory_token" <<<"$recalled"

    echo "==> Live model command with full access"
    proof_file="$test_root/full-access-command-proof"
    "$yo_bin" permissions full-access --yes
    YO_SESSION_ID="yo-live-command" \
        "$yo_bin" ask --private \
        "Run exactly this command using the command tool: touch $proof_file. Then report whether it succeeded."
    test -f "$proof_file"
    "$yo_bin" permissions safe
else
    echo "==> Live Gateway setup skipped (use YO_LIVE_TEST=1 to enable it)"
fi

echo "==> Local Yo checks passed"
