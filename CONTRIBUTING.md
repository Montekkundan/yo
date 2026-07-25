# Contributing

Yo uses one pinned stable Rust toolchain and keeps correctness rules in the repository so local development and CI agree.

## Setup

Install Rust through [rustup](https://rustup.rs/). Entering the repository automatically selects the version in `rust-toolchain.toml` and installs Rustfmt and Clippy.

Install the supply-chain tools once:

```sh
cargo install cargo-audit cargo-deny
```

Linux contributors also need Bubblewrap to run the real sandbox boundary test.

## Before opening a change

Run:

```sh
./scripts/check.sh
```

The check covers formatting, Clippy, Rustdoc, tests, dependency policy, packaging, and the release build. CI repeats the portable checks on Linux, macOS, and Windows and runs `actionlint` over every GitHub Actions workflow.

## Code style

- Let Rustfmt own layout; do not hand-format around it.
- Keep Clippy clean with warnings denied. Use a narrow `#[allow(...)]` only when the reason is documented beside it.
- Unsafe Rust is denied in application code. Test-only process-environment mutation uses narrow, documented exceptions and global serialization.
- Keep modules focused and prefer existing boundaries over new abstractions.
- Add regression tests before fixing behavior that previously failed.
- Never write credentials, prompts, command output, memory content, or local paths to diagnostics.

## Documentation

- Start substantial modules with `//!` documentation describing their responsibility and security boundary.
- Use `///` on public APIs whose behavior, side effects, failure modes, or invariants are not obvious from the signature.
- Use backticks for code identifiers and valid intra-doc links for related Rust items.
- Update the README when a user-facing command or setup requirement changes.
- Rustdoc warnings are errors in CI.

Yo is distributed as a CLI, not as a stable Rust SDK. The library surface exists so the binary and integration tests share the same implementation. Global `missing_docs` enforcement should be enabled only after that surface is narrowed and declared stable; until then, document every non-obvious public behavior and security invariant.

## Commits

Keep commits scoped, explain why the change is needed, and include the checks used to validate it.
