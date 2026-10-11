# Contributing to NeuroWatch

Thanks for helping. Bug reports, ideas and pull requests are welcome.

## Before you start

- **Bugs and ideas:** open an [issue](https://github.com/ttheekw/Neurowatch/issues).
  For a larger change, open an issue first so we can agree on the approach.
- **Security problems:** do not open a public issue. Report them privately
  through GitHub: **Security → Report a vulnerability**.

## Contributor License Agreement

Every contributor signs the [Contributor License Agreement](CLA.md) once. It
lets us ship your contribution both in the open-source app (AGPL-3.0-or-later)
and in our official builds, which also contain proprietary parts such as
NeuroWatch Cloud. You keep the copyright in your work.

On your first pull request a bot asks you to sign by replying:

> I have read the CLA Document and I hereby sign the CLA

A pull request cannot be merged until its author has signed.

## The open-source app and NeuroWatch Cloud

This repository is the complete open-source app: live view, recording,
playback, export, discovery and headless mode, with no camera limit.
NeuroWatch Cloud is a separate, proprietary product; its code is not here.
Official builds add it through two extension points that anyone can use:

- the backend's `nw_backend::extension` (Rust: frame and keyframe taps,
  extra IPC methods, startup and shutdown hooks);
- the desktop app's `neurowatch.extensions` (Python: menu entries, status
  indicators, tray details, headless commands).

Keep the app working without any extension: never assume one is installed.

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
