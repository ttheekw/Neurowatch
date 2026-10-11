"""Summarise test results as Markdown for the CI run's summary page.

Reads pytest JUnit XML reports (``--junitxml``) and ``cargo test`` /
``cargo llvm-cov`` console output, and prints a table of passed, failed and
skipped tests per suite, followed by the names of the tests that failed:

    python scripts/ci_test_summary.py --title "Python" reports/*.xml >> "$GITHUB_STEP_SUMMARY"
    python scripts/ci_test_summary.py --title "Rust" --cargo reports/cargo-test.txt >> ...

Exits 0 even when tests failed: the test steps themselves fail the job.
"""

from __future__ import annotations

import argparse
import re
import sys
import xml.etree.ElementTree as ElementTree
from dataclasses import dataclass, field
from pathlib import Path

_CARGO_RESULT = re.compile(
    r"^test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored", re.MULTILINE
)
_CARGO_FAILED = re.compile(r"^test (\S+) \.\.\. FAILED$", re.MULTILINE)
_CARGO_SUITE = re.compile(r"^\s+Running (?:unittests )?(\S+)", re.MULTILINE)


@dataclass
class Suite:
    name: str
    passed: int = 0
    failed: int = 0
    skipped: int = 0
    failures: list[str] = field(default_factory=list)

    @property
    def total(self) -> int:
        return self.passed + self.failed + self.skipped


def read_junit(path: Path) -> Suite:
    """One pytest JUnit report."""
    suite = Suite(path.stem)
    root = ElementTree.parse(path).getroot()
    for case in root.iter("testcase"):
        name = f"{case.get('classname', '')}::{case.get('name', '')}".strip(":")
        if case.find("failure") is not None or case.find("error") is not None:
            suite.failed += 1
            suite.failures.append(name)
        elif case.find("skipped") is not None:
            suite.skipped += 1
        else:
            suite.passed += 1
    return suite


def read_cargo(path: Path) -> Suite:
    """``cargo test`` output: every test binary's result line, added up."""
    text = path.read_text(errors="replace")
    suite = Suite(path.stem)
    for passed, failed, ignored in _CARGO_RESULT.findall(text):
        suite.passed += int(passed)
        suite.failed += int(failed)
        suite.skipped += int(ignored)
    suite.failures = sorted(set(_CARGO_FAILED.findall(text)))
    if not _CARGO_SUITE.search(text) and not suite.total:
        suite.failures.append("no test results found: the build or the run failed")
    return suite


def markdown(title: str, suites: list[Suite]) -> str:
    failed = sum(suite.failed for suite in suites)
    broken = any(suite.failures for suite in suites)
    status = "❌ failed" if failed or broken else "✅ passed"
    lines = [
        f"## {title} tests: {status}",
        "",
        "| Suite | Passed | Failed | Skipped | Total |",
        "|---|---:|---:|---:|---:|",
    ]
    for suite in suites:
        lines.append(
            f"| {suite.name} | {suite.passed} | {suite.failed} | {suite.skipped} | {suite.total} |"
        )
    if len(suites) > 1:
        passed = sum(s.passed for s in suites)
        skipped = sum(s.skipped for s in suites)
        total = sum(s.total for s in suites)
        lines.append(f"| **All** | **{passed}** | **{failed}** | **{skipped}** | **{total}** |")
    failures = [f"- `{name}`" for suite in suites for name in suite.failures]
    if failures:
        lines += ["", "**Failed:**", "", *failures[:50]]
        if len(failures) > 50:
            lines.append(f"- … and {len(failures) - 50} more")
    return "\n".join(lines) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--title", required=True, help="heading, e.g. Python or Rust")
    parser.add_argument("--cargo", action="append", default=[], type=Path, help="cargo output")
    parser.add_argument("junit", nargs="*", type=Path, help="pytest JUnit XML reports")
    args = parser.parse_args(argv)
    suites: list[Suite] = []
    for path in args.junit:
        suites.append(
            read_junit(path) if path.is_file() else Suite(path.stem, failures=["report missing"])
        )
    for path in args.cargo:
        suites.append(
            read_cargo(path) if path.is_file() else Suite(path.stem, failures=["output missing"])
        )
    sys.stdout.write(markdown(args.title, suites))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
