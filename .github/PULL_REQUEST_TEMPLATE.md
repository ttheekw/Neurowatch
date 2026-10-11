## Summary
<!-- 1-3 bullet points -->

## Type of Change
- [ ] feat — new feature
- [ ] fix — bug fix
- [ ] chore — dependency/tooling update
- [ ] docs — documentation only
- [ ] refactor — no behaviour change
- [ ] perf — performance improvement

## How to Test
<!-- Step-by-step instructions -->

## Pre-Merge Checklist
- [ ] I have signed the [Contributor License Agreement](../CLA.md) (the CLA bot asks on your first pull request)
- [ ] `cargo fmt --all` passes
- [ ] `cargo clippy -- -D warnings` passes
- [ ] `cargo test --all-features` passes
- [ ] `ruff check` and `ruff format --check` pass
- [ ] No `unwrap()`/`expect()` in non-test Rust code
- [ ] No `time.sleep()` on Qt main thread
