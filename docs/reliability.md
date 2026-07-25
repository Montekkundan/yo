# Reliability targets

Yo publishes the following pre-release service-level objectives (SLOs). Credential-free metrics are enforced by nightly CI. Metrics that require a paid Gateway are checked locally by the maintainer before a release.

| Metric | Target | Measurement |
| --- | ---: | --- |
| Tool success rate | >= 95% | Successful terminal command executions / measured command attempts; live suites include model `run_command` attempts |
| Memory precision | >= 90% | Relevant memories / all memories returned by the checked-in retrieval cases |
| Secret-retention failures | 0 | Raw declared secrets found after redaction or in model/tool eval output |
| Startup time p95 | <= 150 ms | Ten `yo --version` process launches measured by the eval runner |
| Gateway latency p95 | <= 15,000 ms | End-to-end duration of each live agent eval, including tool round trips |

These are regression gates, not uptime guarantees for third-party model providers. The JSON report includes `null` for metrics that a selected suite cannot measure. Missing samples do not count as a pass when `YO_EVAL_REQUIRE_ALL_TARGETS=1` is set for the maintainer's pre-release live run.

## Suites

- `evals/offline/memory`: deterministic SQLite FTS and vector retrieval precision and recall.
- `evals/offline/security`: deterministic secret-redaction checks.
- `evals/offline/terminal`: a failed-command to successful-diagnostic recovery workflow.
- `evals/adversarial`: live prompt-injection, destructive-command containment, and automatic-memory filtering.
- `evals/terminal`: live real-command workflows such as Node/nvm inspection and model-driven recovery from pasted terminal failures.

Run the offline gate without credentials:

```sh
cargo run -- eval offline --json
```

Run a live suite with the configured Gateway:

```sh
cargo run -- eval terminal adversarial --json
```

Override the selected model without changing the user's config:

```sh
YO_EVAL_MODEL=anthropic/claude-sonnet-4.6 cargo run -- eval terminal --json
```

The eval runner measures startup automatically. For reproducible experiments, replace that measurement with comma-separated milliseconds:

```sh
YO_EVAL_STARTUP_SAMPLES_MS=22,20,19,21 cargo run -- eval offline --json
```

Nightly CI runs only the deterministic offline suite and needs no model credential. It sets `YO_EVAL_REQUIRE_OFFLINE_TARGETS=1`, gates tool execution, memory precision, secret retention, terminal recovery, startup time, and missing prerequisites, then uploads a machine-readable report. Gateway latency is the only target exempt from that credential-free gate.

Live multi-model checks intentionally run on the maintainer's machine so Gateway credentials never need to be stored in GitHub Actions. Before tagging a release, run the full suite once per supported model:

```sh
YO_EVAL_REQUIRE_ALL_TARGETS=1 \
YO_EVAL_MODEL=anthropic/claude-sonnet-4.6 \
cargo run --release -- eval --json > eval-claude.json

YO_EVAL_REQUIRE_ALL_TARGETS=1 \
YO_EVAL_MODEL=openai/gpt-5.4-mini \
cargo run --release -- eval --json > eval-openai.json

YO_EVAL_REQUIRE_ALL_TARGETS=1 \
YO_EVAL_MODEL=google/gemini-3-flash \
cargo run --release -- eval --json > eval-gemini.json
```

Keep those reports locally as release evidence; they can contain operational metadata and should be inspected before sharing.
