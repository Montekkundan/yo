# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Route all chat, tool, structured-output, and embedding requests through Vercel AI Gateway.
- Add terminal-session chats, command execution with captured results, terminal Markdown rendering, local hybrid memory, `personalize.md`, and a native settings TUI.
- Make `yo setup` automatically select live chat and embedding models, verify tool calling, strict structured output, and embeddings with real requests, initialize local state, and install shell integration idempotently.
- Harden model command authorization with exact-command consent and timeouts, bound automatic-memory retries, and preserve privacy when chats are cleared.
- Add `safe`, `always-ask`, and explicitly confirmed `full-access` model command permission modes, with deterministic and live command/memory retention tests.
- Add file-based live workflow evals with tool-call, command, exit-code, stdout, and reply gates plus exact per-case command allowlists.
- Show TTY-aware Gateway and command progress with the latest redacted shell output, use lightweight pyenv/nvm command bootstraps, and prevent approved commands or leftover child processes from hanging capture.
- Keep successful command answers minimal and remove background highlighting from terminal code rendering.
- Gate GitHub releases on successful builds for every release target.
- Remove direct-provider and local-model runtime integrations.

## [1.0.1] - 2025-04-25
### Changed
- Automated release

## [1.0.0] - 2025-04-24
### Added
- First public release
- Precompiled binaries for macOS, Linux, and Windows
- Homebrew, Scoop, Chocolatey, and AUR installation instructions
- OpenAI and Ollama backend support
- CLI: `yo <question>` and `yo ask <question>`
- Configuration support
