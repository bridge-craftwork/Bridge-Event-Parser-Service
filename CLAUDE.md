# CLAUDE.md

This file provides guidance to Claude Code when working with this repository.

## Project Overview

`bridge-event-parser-service` is a containerized HTTP service that parses bridge event files (PBN/LIN) via the `bridge-parsers` crate. Deploy workflow lives in the `justfile` (same droplet pattern as the other bridge-craftwork services).

## Build & Test Commands

**Use `./dev-build.sh` for local development builds, not bare cargo.** This repo depends on the sibling `bridge-parsers` crate (and transitively `bridge-types`/`bridge-encodings`) as git dependencies, with gitignored `[patch]` overrides in `.cargo/config.toml` redirecting them to the local checkouts in `../`. Cargo never lets a `[patch]` override an existing `Cargo.lock` pin, so bare `cargo build` silently compiles the GitHub revisions of those crates instead of your local edits — and if the patches do take effect, they rewrite `Cargo.lock` with local-path entries that must never be committed (CI has no sibling checkouts). The script keeps a separate local lock (`.cargo/dev.lock`), swaps it in around the cargo call, verifies each patched crate resolved to a local checkout, and leaves the committed `Cargo.lock` untouched.

```bash
./dev-build.sh run                # run the service (= just dev)
./dev-build.sh test               # cargo test (= just test)
./dev-build.sh clippy -- -D warnings   # lint
cargo fmt --check                 # no dependency resolution; bare cargo is fine
```

For CI-parity builds (pre-commit checks) use `./dev-build.sh --ci test` (any cargo subcommand works after `--ci`) — it temporarily disables the local patches and builds with the committed lock's git pins. **Avoid bare cargo for anything that resolves dependencies** (build/test/check/run): with the patches present, a same-version patch is applied immediately and silently rewrites `Cargo.lock` to local-path entries, while a version mismatch makes the patches silently ignored — both wrong. The committed `Cargo.lock` must always pin `git+https://` sources for the internal crates; never commit a lock where those entries have lost their `source =` lines.

Docker images and CI always build the pinned, pushed revisions of the internal crates. To get local sibling changes into an image: push the sibling, then `./dev-build.sh --ci update -p <crate>` to re-pin, and commit the lock.

## Pre-commit Requirements

Before committing, always run and fix:
1. `cargo fmt --all` - Format all code
2. `./dev-build.sh --ci clippy -- -D warnings` - Fix all clippy warnings
3. `./dev-build.sh --ci test` - Ensure all tests pass (CI parity: patches disabled, committed lock's git pins)

## Git Configuration

Use SSH for all GitHub operations:
- Remote: `git@github.com:bridge-craftwork/Bridge-Event-Parser-Service.git`

## Related Projects

All located at `/Users/rick/Development/GitHub/`:

- `../Bridge-Parsers` — PBN/LIN parsing (git dependency; local dev via gitignored `.cargo/config.toml` patch + `./dev-build.sh`)
- `../bridge-types`, `../bridge-encodings` — transitive internal deps, patched the same way
