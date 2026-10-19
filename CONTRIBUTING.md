# Contributing to NeuroWatch

Thanks for helping. Bug reports, ideas and pull requests are welcome.

## Before you start

- **Bugs and ideas:** open an [issue](https://github.com/ttheekw/Neurowatch/issues).
  For a larger change, open an issue first so we can agree on the approach.
- **Security problems:** do not open a public issue. Report them privately
  through GitHub: **Security → Report a vulnerability**.

## Licence of contributions

NeuroWatch is licensed under the [Apache License 2.0](LICENSE). As section 5
of the License says, a contribution you submit is licensed under the same
terms, unless you clearly state otherwise. You keep the copyright in your work.

## The open-source app and NeuroWatch Cloud

This repository is the whole of NeuroWatch: live view, recording, playback,
export, discovery and headless mode, with no camera limit, and the NeuroWatch
Cloud client: `crates/nw-backend/src/cloud` in the backend and
`ui/neurowatch/cloud` in the desktop app (Settings → Cloud, the status bar
light, the tray's cloud state and the `neurowatch cloud` commands).
`crates/nw-cloud-protocol` holds the types shared with the cloud server
(`crates/nw-api`).

Keep the app working without the cloud: it must never be required for local
recording, live view or playback.

## Development setup

On Ubuntu or Debian:

```bash
./neurowatch.sh --bootstrap --system --yes   # build tools and libraries (uses sudo)
./neurowatch.sh                              # build the backend and open the app
```

## Checks before a pull request

GitHub runs these on every pull request and every merge to `main` (the CI
workflow); each run's summary page lists the passed, failed and skipped tests.
Run them locally first:

```bash
cargo fmt --all
cargo clippy --workspace -- -D warnings
cargo test --workspace
uv run ruff check ui/ && uv run ruff format --check ui/
uv run mypy ui/neurowatch/
QT_QPA_PLATFORM=offscreen uv run pytest
```

## Conventions

- Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/):
  `type(scope): summary`, for example `fix(ui): keep the wall size on GNOME`.
- Describe what users will notice in the pull request; release notes are written from it.
- Rust: no `unwrap()`/`expect()` outside tests and startup, no `panic!()` in
  library code, never hold a lock across `.await`.
- Python: type hints on public functions, never block the Qt main thread, and
  `headless.py` and `service/` must not import Qt.
