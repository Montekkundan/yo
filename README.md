# yo

[![CI](https://github.com/Montekkundan/yo/actions/workflows/ci.yml/badge.svg)](https://github.com/Montekkundan/yo/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/yo.svg)](https://crates.io/crates/yo)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE)

Yo is a personal AI assistant for your terminal. It can use [Vercel AI Gateway](https://vercel.com/docs/ai-gateway), [LLM Gateway](https://docs.llmgateway.io), or [OpenRouter](https://openrouter.ai/docs) for model requests, while chats, terminal context, personalization, diagnostics, and durable memory stay on your machine.

## What it does

- Ask with `yo <question>` or `yo ask <question>`
- Choose a tool- and structured-output-compatible model from one supported gateway
- Render Markdown with terminal-aware wrapping, code blocks, lists, and colors
- Run commands inside OS-enforced filesystem and network scopes
- Keep a separate chat for each terminal session
- Recall compact global and repository-specific memories across sessions
- Follow your own `personalize.md` response instructions
- Manage settings, chats, and memories from a native Rust TUI

Yo does not contain direct model-provider integrations or a local-model backend. Chat, structured memory extraction, and embeddings all use the gateway selected during setup.

## Install

Precompiled binaries are available from the [Releases page](https://github.com/montekkundan/yo/releases) for Apple Silicon and Intel macOS, ARM64 and x86-64 Linux, and x86-64 Windows.

With Homebrew:

```sh
brew tap Montekkundan/yo https://github.com/Montekkundan/yo.git
brew install Montekkundan/yo/yo-bin
```

With Cargo:

```sh
cargo install yo
```

Or from this repository:

```sh
cargo install --path .
```

## Set up a model gateway

Run:

```sh
yo setup
```

Yo asks you to choose Vercel AI Gateway, LLM Gateway, or OpenRouter. You can also select one non-interactively:

```sh
yo setup --provider vercel
yo setup --provider llm-gateway
yo setup --provider open-router
```

If Yo cannot find that gateway's environment variable, it asks for a key and stores it in the operating system credential store—never in `config.toml` or SQLite. It then:

- loads the selected gateway's live model catalog;
- keeps a valid existing model or selects a recommended live chat and embedding model;
- makes tiny tool-call, structured-output, and embedding requests to verify the credential, credits, and models;
- initializes local chats, memory, and `personalize.md`; and
- installs the per-terminal shell hook in zsh, bash, or fish when possible.

Rerunning `yo setup` is safe and does not duplicate the shell hook. Use `yo models` and `yo model creator/model` if you want a different chat model later.

Use `yo gateway status`, `yo gateway set`, or `yo gateway delete` to inspect, replace, or remove the stored credential without ever printing it.

For CI or temporary use, set the variable for your selected gateway:

```sh
# Vercel AI Gateway
export AI_GATEWAY_API_KEY='...'
# or Vercel OIDC
export VERCEL_OIDC_TOKEN='...'

# LLM Gateway
export LLM_GATEWAY_API_KEY='...'

# OpenRouter
export OPENROUTER_API_KEY='...'
```

## One chat per terminal

`yo setup` normally installs this automatically. If your shell is not detected or you prefer to configure it manually, add one line to your startup file:

```sh
# zsh
eval "$(yo init zsh)"

# bash
eval "$(yo init bash)"

# fish
yo init fish | source
```

The same terminal keeps its chat. Opening a new terminal creates a new session and chat. Yo also has a best-effort fallback when the shell initialization is missing.

## Ask and run commands

```sh
yo what is the command for listing nvm versions
yo can you run that command
```

The model can call Yo's command tool. A recognized read-only command written directly in the same request can run immediately. Requests such as “run that,” model-invented commands, and mutating or dangerous commands require confirmation.

You can bypass the model and run a command explicitly:

```sh
yo run -- nvm list
yo run -- cargo test --all
```

`yo run` uses a lightweight login shell for normal executables, minimal pyenv/nvm bootstraps for managed runtimes, and falls back to the interactive shell for custom functions. Yo captures bounded stdout/stderr, exit status, working directory, and duration, redacts likely credentials, prints the result, and makes it available to the next question in that terminal.

Interactive terminals show a single-line spinner with the latest redacted output while Gateway requests and commands are running. Captured commands are non-interactive and receive closed stdin, so commands that need their own prompt should be run directly in the terminal.

Successful command questions default to the requested result only. Yo omits routine exit-code, timing, success, and emoji commentary unless you ask for those details.

Choose how model-proposed commands are approved:

```sh
yo permissions
yo permissions safe
yo permissions always-ask
yo permissions full-access
```

- `safe` allows only an exact, recognized read-only command from the current request without asking. Unknown, sensitive, mutating, or dangerous commands prompt for `y/N`.
- `always-ask` prompts for every model-proposed command.
- `full-access` never prompts. Enabling it requires an explicit warning confirmation. OS sandbox scopes still apply.

`yo run -- ...` is always a direct user instruction and is not affected by these approval modes. It is still subject to the configured OS sandbox.

### Command sandbox

On macOS, Yo uses Seatbelt. On Linux, it uses Bubblewrap (`bwrap`). Model-proposed commands fail closed when isolation is unavailable; explicit `yo run` commands use best-effort isolation in `auto` mode. Use `required` when even explicit commands must fail closed.

The current working directory is writable by default, additional scopes are opt-in, and child processes cannot see gateway keys or common secret environment variables. Network access is denied by default.

```sh
yo sandbox status
yo sandbox mode required
yo sandbox add-read ~/Developer/shared
mkdir -p /tmp/yo-output
yo sandbox add-write /tmp/yo-output
yo sandbox network allow
yo sandbox clear-scopes
yo sandbox reset
```

Install Bubblewrap with your Linux package manager if `yo doctor` reports that it is missing. Windows currently has no supported OS sandbox backend, so `required` mode and model-proposed commands fail closed there.

A normal CLI cannot portably read arbitrary terminal scrollback. The shell integration supplies the previous command, exit status, and directory. Use `yo run`, or pipe real output into a question, when Yo needs the exact error text:

```sh
some-command 2>&1 | yo why did this fail
```

## Workflow evals

Yo includes Rust-native, file-based evals inspired by [Eve's eval model](https://eve.dev/docs/evals/overview). They run the configured Gateway model through Yo's real agent/tool loop and gate on what actually happened: the tool name, command, exit code, stdout, and final reply.

```sh
# Use this checkout even if another yo is installed
cargo run -- eval --list
cargo run -- eval terminal/node-version --verbose
cargo run -- eval terminal --json
cargo run -- eval offline --json
cargo run -- eval adversarial --json
```

Eval ids come from files under `evals/`, so `evals/terminal/nvm-list.eval.toml` becomes `terminal/nvm-list`. A directory prefix runs the whole group. A failed gate exits non-zero; a missing local prerequisite such as `nvm` is reported as skipped.

Each case runs with an in-memory chat database, memory and terminal-history recall disabled, and an exact command allowlist. If the model proposes anything outside `allowed_commands`, Yo denies it and the eval fails. Normal chats, memories, settings, and permission mode are not changed.

Example case:

```toml
description = "Lists installed Node.js versions through nvm."
prompt = "Show all Node.js versions installed through nvm and identify the active one."
tags = ["terminal", "tools", "node"]
requires = ["nvm"]
allowed_commands = ["nvm list", "nvm ls"]

[expect]
tool = "run_command"
commands = ["nvm list", "nvm ls"]
exit_code = 0
stdout_nonempty = true
reply_nonempty = true
```

Running an eval performs live Gateway requests and may consume a small amount of credit. `yo eval --list` does not require a credential or call a model.

The deterministic offline suites cover FTS and vector memory precision/recall, secret redaction, and failed-command recovery. Live cases cover automatic-memory filtering, prompt injection, destructive-command containment, real command use, and model-driven recovery from terminal errors. Nightly CI runs the credential-free offline gate. Maintainers run the paid multi-model suite locally before a release, so no Gateway key needs to be stored in GitHub Actions.

## Terminal formatting

AI responses are Markdown. Interactive terminals receive wrapped, styled output similar in spirit to [Glow](https://github.com/charmbracelet/glow), implemented directly in the Rust binary. Redirected output stays as portable Markdown. `NO_COLOR=1`, `CLICOLOR=0`, and `TERM=dumb` are respected.

## Manage Yo

Open the native terminal interface for the quickest overview:

```sh
yo settings
```

It has **Overview**, **Chats**, **Memory**, and **Personalize** tabs. Use:

- `←` / `→` or Tab to change tabs;
- `↑` / `↓` to select an item;
- `a` to cycle command approval modes;
- `m` to toggle durable memory;
- `t` to toggle terminal context;
- `e` to edit `personalize.md` from the Personalize tab;
- `d` twice to delete the selected chat or memory; and
- `q` or Esc to quit.

Every setting also has a normal CLI command, so Yo remains scriptable and usable in a minimal terminal.

### Health, updates, backups, and diagnostics

```sh
yo doctor                 # credentials, gateway, DB, sandbox, updater, local files
yo doctor --offline       # skip Gateway and release-network checks
yo doctor --json          # machine-readable health report

yo update --check
yo update                 # verify SHA-256 plus Sigstore/GitHub provenance, then replace Yo

yo database integrity
yo database backup
yo database backup ./yo-backup.db
yo database repair        # creates an untouched backup before repair
```

`yo update` refuses to replace the binary unless either `cosign` validates the release workflow's Sigstore identity or `gh attestation verify` validates GitHub provenance. Package-manager installations can still be upgraded through their package manager.

Diagnostics are off by default and stay local. They contain fixed event names, success booleans, timings, provider names, and HTTP status codes—never prompts, command text, output, paths, memories, model replies, or credentials.

```sh
yo diagnostics status
yo diagnostics on
yo diagnostics export
yo diagnostics clear
yo diagnostics off
```

### Chats

```sh
yo new --title debugging       # start a new chat in this terminal
yo chats                       # list saved chats and their ids
yo chat 3                      # switch this terminal to chat 3
yo view-chat                   # print the current chat
yo search docker error         # search all chat messages
yo clear-history               # clear the current chat
yo delete-chat 3               # delete one chat
yo clear-all-chats             # delete every local chat
```

The same terminal continues the same chat. A newly opened terminal gets a new session and chat, while durable memory remains available across all sessions.

### Memory

Chats and durable memory are stored in SQLite. Retrieval combines FTS5 keyword search with exact cosine vector search, keeping a small personal memory store simple and fast. Yo automatically saves only a few compact facts per turn; it does not promote raw terminal logs, credentials, or sensitive extracted items.

```sh
yo remember this project uses pnpm
yo memory list
yo memory search package manager
yo memory edit 12 this project uses bun
yo memory forget 12
yo memory export
yo memory reindex
yo memory off
yo memory on
yo memory clear
yo memory purge
```

`clear` removes all durable memory records. `purge` also checkpoints and compacts the database so deleted memory pages are reclaimed. Neither command is required for deleting ordinary chats.

Use private mode when one request should not recall or create durable memories:

```sh
yo ask --private what does this error mean
```

Private requests still belong to the current local chat; the flag controls durable cross-session memory for that request.

### Personalization, models, and permissions

```sh
yo personalize show
yo personalize path
yo personalize edit
yo personalize add be concise and casual
yo personalize reset

yo current
yo models
yo model anthropic/claude-sonnet-4.6

yo permissions
yo permissions safe
yo permissions always-ask
yo permissions full-access
```

Yo follows the private `personalize.md` file on every request. You can also say `yo be more casual from now on`; when a request is explicitly ongoing, the model can append one instruction through a restricted tool that cannot select or overwrite another file.

`yo current` summarizes the active model, chat, memory, terminal context, and command approval mode. `yo config` prints the local configuration, database, and `personalize.md` paths without exposing the Gateway credential.

## Security and privacy

- Gateway credentials live in the OS credential store or environment.
- Vercel requests set `disallowPromptTraining`; privacy and retention controls for other gateways follow the selected gateway account and provider policy.
- Local configuration and data use owner-only permissions where supported.
- Captured command output is size-limited and redacted before storage or model use.
- Commands use Seatbelt on macOS or Bubblewrap on Linux with explicit filesystem/network scopes. Model commands fail closed without a backend unless sandboxing is deliberately disabled.
- Approval policy and OS isolation are independent: `full-access` skips prompts but does not disable the sandbox.
- Memory deletion removes its text, embedding, FTS row, and provenance. `yo memory purge` also checkpoints and vacuums SQLite.
- Existing installations are migrated; legacy plaintext credentials are removed from configuration.

Release archives are smoke-tested on every published target, checksummed, keylessly signed through Sigstore, accompanied by a CycloneDX SBOM, and attested by GitHub Actions. CI also runs `cargo audit` and `cargo deny`.

GitHub releases use the built-in `GITHUB_TOKEN` and OIDC identity, so they need no custom secret. Add `CARGO_REGISTRY_TOKEN` only when publishing the crate to crates.io. `DISTRIBUTION_TOKEN` is only needed for the optional AUR and Scoop publication workflows. Gateway keys stay local: nightly CI is deterministic and credential-free, while maintainers run paid multi-model evals locally before tagging.

Verify a downloaded release manually:

```sh
sha256sum -c SHA256SUMS
gh attestation verify yo-2.0.0-x86_64-unknown-linux-musl.tar.gz \
  -R Montekkundan/yo \
  --signer-workflow Montekkundan/yo/.github/workflows/release.yml \
  --source-ref refs/tags/2.0.0 \
  --cert-identity "https://github.com/Montekkundan/yo/.github/workflows/release.yml@refs/tags/2.0.0" \
  --deny-self-hosted-runners
cosign verify-blob yo-2.0.0-x86_64-unknown-linux-musl.tar.gz \
  --bundle yo-2.0.0-x86_64-unknown-linux-musl.tar.gz.sigstore.json \
  --certificate-identity "https://github.com/Montekkundan/yo/.github/workflows/release.yml@refs/tags/2.0.0" \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

The release gates and measurable targets for tool success, memory precision, secret retention, startup time, and Gateway latency are documented in [docs/reliability.md](docs/reliability.md).

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo doc --locked --no-deps --all-features --document-private-items
cargo test --all
cargo audit
cargo deny check
cargo build --release
cargo package --locked
```

Or run the complete local quality gate:

```sh
./scripts/check.sh
```

The repository pins Rust 1.91.1, denies unsafe application code and Rustdoc errors, treats all standard Clippy warnings as errors, pins third-party workflow actions to immutable revisions, and uses checked-in Rustfmt and EditorConfig rules. See [CONTRIBUTING.md](CONTRIBUTING.md) for the code and documentation conventions.

### Test this checkout without using an installed `yo`

Run the local test harness:

```sh
./scripts/test-local.sh
```

It always invokes `./target/debug/yo`, creates a temporary config/database, and leaves an already installed `yo` untouched. The normal run skips paid/live Gateway requests.

To include isolated end-to-end setup, live workflow evals, automatic cross-session memory recall, and model command execution:

```sh
export AI_GATEWAY_API_KEY='your-key'
YO_LIVE_TEST=1 ./scripts/test-local.sh
```

The harness also detects `LLM_GATEWAY_API_KEY` and `OPENROUTER_API_KEY`. Set
`YO_TEST_PROVIDER=llm-gateway` or `YO_TEST_PROVIDER=open-router` to choose one
explicitly when more than one credential is present.

The live test stores and recalls a unique memory from a different terminal session, then lets the model create a proof file under the temporary test directory. It may consume a small amount of Gateway credit.

For individual commands, `cargo run --` also guarantees that this checkout is used:

```sh
cargo run -- --version
cargo run -- setup
cargo run -- current
cargo run -- run -- nvm list
cargo run -- what is this project
```

## License

[MIT](./LICENSE)
