# Integration Test with Testcontainers-rs

## Objective

Create CLI-level integration tests using testcontainers-rs with a real ScyllaDB instance.
Tests invoke the `latte` binary as a subprocess (like a user would), validating the full stack end-to-end.

## Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Testcontainers API | Async (`tokio` runner) | Project is tokio-based; avoids `blocking` feature overhead |
| Wait strategy | TCP connect loop on CQL port (9042) with backoff | Reliable (~5-15s), no log-message fragility, no wasteful fixed sleep |
| ScyllaDB image registry | configurable via 'SCYLLADB_IMAGE_REGISTRY', default is `scylladb/scylla` | ScyllaDB image registry |
| ScyllaDB image tag | configurable via 'SCYLLADB_IMAGE_TAG', default is `2026.1` | ScyllaDB image tag |
| Test level | CLI (subprocess invocation of `latte` binary) | End-to-end validation, tests what users actually run |
| Assertions | Exit code 0 + stdout contains throughput metrics | Meaningful validation beyond "something printed" |
| Test attribute | `#[ignore]` | Don't slow down regular `cargo test` |
| CI trigger | push to main + PRs + manual `workflow_dispatch` | Flexibility for on-demand runs |

## Implementation

### Files Modified

- `Cargo.toml` – add `testcontainers = "0.23"` to dev-dependencies (no `blocking` feature)

### Files Created

- `tests/integration_test.rs` – async integration test
- `.github/workflows/integration-test.yml` – dedicated CI workflow

### Test Strategy

1. Start `scylladb/scylla-enterprise:{version}` container via testcontainers async API
2. Wait for CQL port readiness using TCP connect loop (max 60s, 2s interval)
3. Build latte in release mode (within the test, leveraging cargo caching)
4. Invoke `latte run workloads/basic/write.rn --hosts 127.0.0.1:{port} --duration 10s --warmup 0s`
5. Assert: exit code 0, stdout contains throughput indicator

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `SCYLLADB_IMAGE_REGISTRY` | `scylladb/scylla` | ScyllaDB image registry |
| `SCYLLADB_IMAGE_TAG` | `2026.1` | ScyllaDB image tag |

### Running Locally

```bash
cargo test --test integration_test -- --ignored --nocapture
```

### Success Criteria

- ScyllaDB container starts and becomes ready
- `latte schema` command runs successfully
- `latte run` commands run successfully
- Test passes in GitHub Actions CI
- No changes to existing functionality
