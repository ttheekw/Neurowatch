---
name: Feature Request
about: Propose a new feature or enhancement
labels: ["feature", "needs-triage"]
---

## Feature: <!-- one-line title -->

**Version target:** V<!-- N --> <!-- Codename -->
**Spec section:** <!-- e.g., §2.1 Recording Engine -->
**Priority:** P<!-- 1-3 -->
**Estimated effort:** <!-- XS / S / M / L / XL -->

## Problem Statement
<!-- Who is affected, under what conditions, what goes wrong -->

## Proposed Solution
<!-- What can users do that they couldn't before -->

## Acceptance Criteria
- [ ] <!-- criterion 1 -->
- [ ] <!-- criterion 2 -->
- [ ] Unit tests cover the new logic

## Technical Notes

### Backend (Rust)
<!-- Crates affected, new traits, GStreamer changes, IPC messages -->

### UI (Python)
<!-- New widgets, signals/slots, dialogs -->

### IPC Changes
<!-- New JSON-RPC methods, notification types -->

### Feature Flag
`cargo build --features v<!-- N -->`

## Out of Scope
<!-- Explicitly list what this will NOT do -->
