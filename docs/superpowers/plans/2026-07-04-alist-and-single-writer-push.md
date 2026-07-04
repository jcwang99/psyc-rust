# AList And Single-Writer Push Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `alist+` remotes work against `/dav`-backed AList servers and add an explicit push confirmation path for conservative single-writer backends.

**Architecture:** Normalize AList remote roots during remote-spec parsing, then thread a narrowly-scoped explicit confirmation path through CLI, SDK, and sync publish entrypoints. Default push behavior remains unchanged and safe.

**Tech Stack:** Rust, clap, anyhow, serde, cargo test

## Global Constraints

- Preserve default push safety for conservative single-writer backends.
- Do not change `webdav+` parsing semantics.
- Use TDD: write failing tests before implementation.
- Keep the implementation minimal and compatible with existing call sites.

---

### Task 1: Lock AList parsing behavior with tests

**Files:**
- Modify: `crates/e2v-sync/tests/remote_spec.rs`
- Modify: `crates/e2v-sync/src/remote_spec.rs`

- [ ] Add tests for implicit and explicit `/dav`
- [ ] Run the targeted remote-spec tests and confirm failure
- [ ] Implement AList root normalization
- [ ] Re-run the targeted remote-spec tests and confirm pass

### Task 2: Add explicit risky single-writer push confirmation

**Files:**
- Modify: `crates/e2v-sync/tests/push_remote.rs`
- Modify: `crates/e2v-sync/src/push.rs`
- Modify: `crates/e2v-sync/src/publisher.rs`
- Modify: `crates/e2v-sync/src/lib.rs`
- Modify: `crates/e2v-api/src/lib.rs`
- Modify: `crates/e2v-cli/src/lib.rs`
- Modify: `crates/e2v-cli/tests/cli.rs`

- [ ] Add failing sync and CLI tests for explicit confirmation
- [ ] Run the targeted tests and confirm failure
- [ ] Implement the explicit confirmation path with default behavior unchanged
- [ ] Re-run the targeted tests and confirm pass

### Task 3: Verify end-to-end behavior

**Files:**
- Modify: `docs/psyc-rust-user-manual.zh-CN.md`
- Modify: `docs/psyc-rust-user-manual.en.md`

- [ ] Update the manuals with AList `/dav` compatibility and the push confirmation flag
- [ ] Run focused tests, then wider workspace verification
- [ ] Re-test the live AList path used in this debugging session
- [ ] Prepare commit once verification is green
