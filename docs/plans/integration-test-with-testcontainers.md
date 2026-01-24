# Integration Test with Testcontainers-rs Plan

## Objective
Create a sample integration test using testcontainers-rs with real running ScyllaDB and introduce a GitHub action to run it. The test should run one of the runscript examples for 1 minute.

## Current State Analysis
- Latte is a database benchmarking tool for Apache Cassandra and ScyllaDB
- Written in Rust with existing test infrastructure using cargo-nextest
- Has example workloads in `workloads/basic/` directory (e.g., `write.rn`)
- Existing GitHub Actions workflow in `.github/workflows/rust.yml` for CI
- No current integration tests with real database

## Implementation Plan

### 1. Add testcontainers-rs Dependency
- Add `testcontainers` crate to `[dev-dependencies]` in `Cargo.toml`
- This will allow us to spin up ScyllaDB containers during tests

### 2. Create Integration Test
- Create a new test file: `tests/integration_test.rs`
- Test will:
  - Start a ScyllaDB container using testcontainers
  - Run latte with the `workloads/basic/write.rn` workload for 1 minute
  - Verify the test completes successfully
  - Clean up the container

### 3. Add GitHub Action Workflow
- Create `.github/workflows/integration-test.yml`
- Workflow will:
  - Install Docker (should be available by default on GitHub runners)
  - Run the integration test using `cargo test --test integration_test`
  - Run on pull requests and pushes to main branch

### 4. Test Execution Strategy
- The integration test will use the latte CLI binary
- Execute it with parameters: `--duration 60s` for 1 minute run
- Use the `write.rn` workload as it's simple and self-contained
- Connect to the testcontainer ScyllaDB instance

## Expected Changes

### Files to Create:
1. `docs/plans/integration-test-with-testcontainers.md` (this file)
2. `tests/integration_test.rs` - Integration test file
3. `.github/workflows/integration-test.yml` - GitHub Actions workflow

### Files to Modify:
1. `Cargo.toml` - Add testcontainers to dev-dependencies

## Testing Plan
1. Build the project locally: `cargo build`
2. Run the integration test locally: `cargo test --test integration_test`
3. Verify the GitHub Action runs successfully in CI

## Minimal Changes Approach
- Only add necessary dependencies for testcontainers
- Create single focused integration test
- Reuse existing workload files
- Add minimal GitHub Action configuration

## Success Criteria
- Integration test successfully starts ScyllaDB container
- Latte runs the write workload for 1 minute
- Test passes locally and in GitHub Actions
- No changes to existing functionality
