"""Static contract tests for the CI workflow; nothing is run."""

from __future__ import annotations

import re
from pathlib import Path, PurePosixPath

ROOT = Path(__file__).resolve().parents[1]
CI_WORKFLOW = ROOT / ".github/workflows/ci.yml"


def _run_commands() -> list[str]:
    """Every command line of the workflow's run steps (one-line and block)."""
    commands = re.findall(r"^\s+run: (?!\|)(.+)$", CI_WORKFLOW.read_text(), re.MULTILINE)
    for block in re.findall(r"run: \|\n((?:\s{10,}.*\n?)+)", CI_WORKFLOW.read_text()):
        commands += [line.strip() for line in block.splitlines() if line.strip()]
    return commands


def test_rust_coverage_step_enforces_80_percent_workspace_lines() -> None:
    commands = [cmd for cmd in _run_commands() if cmd.startswith("cargo llvm-cov ")]
    gated = [cmd for cmd in commands if "--fail-under-lines" in cmd]
    # cargo-llvm-cov ignores --fail-under-lines with --no-report, so the gate must be on `report`.
    assert any(
        cmd.startswith("cargo llvm-cov --workspace --all-features --no-report") for cmd in commands
    )
    assert gated == ["cargo llvm-cov report --summary-only --fail-under-lines 85"]


def test_every_merge_to_main_runs_the_tests_and_summarises_them() -> None:
    workflow = CI_WORKFLOW.read_text()
    trigger = workflow[workflow.index("on:") : workflow.index("permissions:")]
    assert "push:" in trigger and "main" in trigger and "pull_request:" in trigger
    commands = _run_commands()
    # The Rust test output is kept (tee, with pipefail) for the summary.
    assert (
        "cargo llvm-cov --workspace --all-features --no-report 2>&1 | tee reports/cargo-test.txt"
        in commands
    )
    assert any("--junitxml=reports/app.xml" in cmd for cmd in commands)
    assert any("tests/test_release_smoke.py" in cmd for cmd in commands)
    summaries = [cmd for cmd in commands if "scripts/ci_test_summary.py" in cmd]
    assert len(summaries) == 2 and all('>> "$GITHUB_STEP_SUMMARY"' in cmd for cmd in summaries)
    # Summaries and report uploads run even when tests failed.
    for step in ("name: Test summary", "name: Upload test"):
        for index in [m.start() for m in re.finditer(re.escape(step), workflow)]:
            assert "if: ${{ !cancelled() }}" in workflow[index : index + 120], step


def test_rust_coverage_artifact_uploads_generated_html_report() -> None:
    workflow = CI_WORKFLOW.read_text()
    output_dir_match = re.search(
        r"cargo llvm-cov report --html --output-dir (?P<path>\S+)",
        workflow,
    )
    artifact_path_match = re.search(
        r"name: rust-coverage\s+path: \|\s+(?P<path>\S+)",
        workflow,
        re.MULTILINE,
    )

    assert output_dir_match is not None
    assert artifact_path_match is not None
    generated_html_path = PurePosixPath(output_dir_match["path"]) / "html"
    artifact_html_path = PurePosixPath(artifact_path_match["path"])
    assert artifact_html_path == generated_html_path


def test_the_summary_counts_pytest_and_cargo_results(tmp_path: Path, monkeypatch) -> None:
    import importlib.util
    import sys

    spec = importlib.util.spec_from_file_location(
        "ci_test_summary", ROOT / "scripts" / "ci_test_summary.py"
    )
    assert spec and spec.loader
    summary = importlib.util.module_from_spec(spec)
    monkeypatch.setitem(sys.modules, "ci_test_summary", summary)  # dataclasses need it
    spec.loader.exec_module(summary)

    junit = tmp_path / "app.xml"
    junit.write_text(
        '<testsuites><testsuite name="pytest">'
        '<testcase classname="tests.ui.test_a" name="test_ok"/>'
        '<testcase classname="tests.ui.test_a" name="test_bad"><failure message="x"/></testcase>'
        '<testcase classname="tests.ui.test_b" name="test_skip"><skipped/></testcase>'
        "</testsuite></testsuites>"
    )
    cargo = tmp_path / "cargo-test.txt"
    cargo.write_text(
        "     Running unittests src/lib.rs (target/debug/deps/nw_backend-1)\n"
        "test a::works ... ok\n"
        "test a::breaks ... FAILED\n"
        "test result: FAILED. 9 passed; 1 failed; 2 ignored; 0 measured; 0 filtered out\n"
        "     Running tests/x.rs (target/debug/deps/x-2)\n"
        "test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n"
    )

    text = summary.markdown("All", [summary.read_junit(junit), summary.read_cargo(cargo)])

    assert "## All tests: ❌ failed" in text
    assert "| app | 1 | 1 | 1 | 3 |" in text
    assert "| cargo-test | 12 | 1 | 2 | 15 |" in text
    assert "| **All** | **13** | **2** | **3** | **18** |" in text
    assert "- `tests.ui.test_a::test_bad`" in text and "- `a::breaks`" in text

    passing = tmp_path / "ok.txt"
    passing.write_text("     Running x\ntest result: ok. 2 passed; 0 failed; 0 ignored;\n")
    assert "✅ passed" in summary.markdown("Rust", [summary.read_cargo(passing)])


def test_workflows_use_actions_that_run_on_node_24() -> None:
    # GitHub retired Node 20 for actions; these majors were its last users here.
    retired = {
        "actions/checkout": 7,
        "actions/setup-python": 7,
        "actions/upload-artifact": 7,
        "actions/download-artifact": 8,
        "astral-sh/setup-uv": 10,
    }
    for workflow in (ROOT / ".github/workflows").glob("*.yml"):
        text = workflow.read_text()
        assert "uses: ilammy/msvc-dev-cmd" not in text, workflow.name  # still Node 20
        for action, minimum in retired.items():
            for major in re.findall(rf"uses: {re.escape(action)}@[0-9a-f]{{40}} # v(\d+)", text):
                assert int(major) >= minimum, f"{workflow.name}: {action}@v{major}"
        # setup-uv publishes no moving major tags (no "v10"): only exact versions.
        for ref in re.findall(r"uses: astral-sh/setup-uv@[0-9a-f]{40} # (\S+)", text):
            assert re.fullmatch(r"v\d+\.\d+\.\d+", ref), f"{workflow.name}: setup-uv@{ref}"


def test_every_action_is_pinned_to_a_commit() -> None:
    # Vulnerability checklist R15: a tag or branch (`dtolnay/rust-toolchain@stable`)
    # can be moved to other code, which then runs in the jobs that build the
    # releases and hold the downloads bucket's keys. Each action names a commit,
    # its version in a comment beside it.
    for workflow in (ROOT / ".github/workflows").glob("*.yml"):
        for line in workflow.read_text().splitlines():
            if "uses:" in line and "uses: ./" not in line:
                assert re.search(r"uses: [\w.-]+/[\w./-]+@[0-9a-f]{40} # \S+$", line), (
                    f"{workflow.name}: {line.strip()}"
                )


def test_clang_comes_from_the_apt_llvm_repository_with_a_pinned_key() -> None:
    """No workflow, and not the bootstrap, runs a downloaded llvm.sh as root: they use
    scripts/install_clang21.sh, which adds the repository only after the signing
    key matches a pinned fingerprint."""
    script = (ROOT / "scripts/install_clang21.sh").read_text()
    assert 'LLVM_KEY_FINGERPRINT="6084F3CF814B57C1CF12EFD515CF4D18AF4F7421"' in script
    assert "Signed-By:" in script and "verify_key" in script
    for path in (
        ROOT / ".github/workflows/ci.yml",
        ROOT / ".github/workflows/release.yml",
    ):
        text = path.read_text()
        assert "llvm.sh" not in text, path
        assert "bash scripts/install_clang21.sh" in text, path
    bootstrap = (ROOT / "neurowatch.sh").read_text()
    assert "llvm.sh" not in bootstrap
    assert "scripts/install_clang21.sh" in bootstrap


def test_the_clang_install_refuses_a_key_with_another_fingerprint(tmp_path: Path) -> None:
    import os
    import shutil
    import subprocess

    import pytest

    if shutil.which("gpg") is None or shutil.which("bash") is None:
        pytest.skip("needs gpg and bash")
    home = tmp_path / "gnupg"
    home.mkdir(mode=0o700)
    env = {**os.environ, "GNUPGHOME": str(home)}
    generate = [
        "gpg", "--batch", "--pinentry-mode", "loopback", "--passphrase", "",
        "--quick-generate-key", "intruder <x@example.org>", "ed25519", "sign", "1d",
    ]  # fmt: skip
    try:
        subprocess.run(generate, env=env, check=True, capture_output=True, timeout=60)
        key = tmp_path / "key.asc"
        key.write_bytes(
            subprocess.run(
                ["gpg", "--armor", "--export", "intruder"], env=env, check=True, capture_output=True
            ).stdout
        )
        result = subprocess.run(
            ["bash", str(ROOT / "scripts/install_clang21.sh"), "--verify-only"],
            env={**env, "NW_LLVM_KEY_FILE": str(key)},
            capture_output=True,
            text=True,
        )
        assert result.returncode != 0 and "not the pinned" in result.stderr
        garbage = tmp_path / "garbage"
        garbage.write_text("not a key")
        result = subprocess.run(
            ["bash", str(ROOT / "scripts/install_clang21.sh"), "--verify-only"],
            env={**env, "NW_LLVM_KEY_FILE": str(garbage)},
            capture_output=True,
            text=True,
        )
        assert result.returncode != 0 and "not a readable OpenPGP key" in result.stderr
    finally:
        # Making the key started a gpg-agent for this home: stop it.
        subprocess.run(["gpgconf", "--kill", "gpg-agent"], env=env, capture_output=True, timeout=30)
