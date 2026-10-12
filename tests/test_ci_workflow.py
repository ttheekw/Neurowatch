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
    assert gated == ["cargo llvm-cov report --summary-only --fail-under-lines 80"]


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
            for major in re.findall(rf"uses: {re.escape(action)}@v(\d+)", text):
                assert int(major) >= minimum, f"{workflow.name}: {action}@v{major}"
        # setup-uv publishes no moving major tags (no "v10"): only exact versions.
        for ref in re.findall(r"uses: astral-sh/setup-uv@(\S+)", text):
            assert re.fullmatch(r"v\d+\.\d+\.\d+", ref), f"{workflow.name}: setup-uv@{ref}"
