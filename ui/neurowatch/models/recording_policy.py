"""Pure validation for local-calendar recording rules. No Qt, RPC or I/O."""

from __future__ import annotations

import re
from copy import deepcopy
from typing import Any

MODES = ("continuous", "events", "off")
DAYS = ("Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun")
POLICY_DEFAULTS: dict[str, Any] = {
    "mode": "continuous",
    "weekly_rules": [],
    "event_pre_sec": 10,
    "event_post_sec": 15,
    "non_event_keep_hours": 0,
}


def normalized_policy(policy: dict) -> dict:
    """Only the fields shared by schedule editing and policy copying."""
    result = deepcopy(POLICY_DEFAULTS)
    result.update({key: deepcopy(policy[key]) for key in result if policy.get(key) is not None})
    return result


def weekly_slots(rules: list[dict]) -> list[str | None]:
    problem = policy_problem({"weekly_rules": rules})
    if problem:
        raise ValueError(problem)
    slots: list[str | None] = [None] * 10080
    for rule in rules:
        start = int(rule["start"][:2]) * 60 + int(rule["start"][3:])
        end = int(rule["end"][:2]) * 60 + int(rule["end"][3:])
        for day in rule["days"]:
            offset = day * 1440 + (0 if start == end else start)
            for minute in range((end - start) % 1440 or 1440):
                slots[(offset + minute) % 10080] = rule["mode"]
    return slots


def rules_from_slots(slots: list[str | None]) -> list[dict]:
    """Group identical daily windows without rounding untouched minutes."""
    if len(slots) != 10080 or any(mode is not None and mode not in MODES for mode in slots):
        raise ValueError("Invalid weekly schedule")
    grouped: dict[tuple[int, int, str], list[int]] = {}
    for day in range(7):
        start = 0
        while start < 1440:
            mode = slots[day * 1440 + start]
            end = start + 1
            while end < 1440 and slots[day * 1440 + end] == mode:
                end += 1
            if mode is not None:
                grouped.setdefault((start, end, mode), []).append(day)
            start = end

    def clock(minute: int) -> str:
        return f"{minute % 1440 // 60:02d}:{minute % 60:02d}"

    rules = [
        {"days": days, "start": clock(start), "end": clock(end), "mode": mode}
        for (start, end, mode), days in grouped.items()
    ]
    problem = policy_problem({"weekly_rules": rules})
    if problem:
        raise ValueError(problem)
    return rules


def paint_windows(
    rules: list[dict], first_day: int, last_day: int, start: int, end: int, mode: str | None
) -> list[dict]:
    if not 0 <= first_day <= last_day < 7 or not 0 <= start < end <= 1440:
        raise ValueError("Select a range inside the week")
    if mode is not None and mode not in MODES:
        raise ValueError("Choose a recording mode")
    slots = weekly_slots(rules)
    for day in range(first_day, last_day + 1):
        slots[day * 1440 + start : day * 1440 + end] = [mode] * (end - start)
    return rules_from_slots(slots)


def policy_problem(policy: dict) -> str | None:
    if policy.get("mode", "continuous") not in MODES:
        return "Choose continuous, event retention, or off."
    for key, default, limit in (
        ("event_pre_sec", 10, 3600),
        ("event_post_sec", 15, 3600),
        ("non_event_keep_hours", 0, 87600),
    ):
        value = policy.get(key, default)
        if type(value) is not int or not 0 <= value <= limit:
            return "Padding must be 0–3600 seconds; non-event history at most ten years."
    rules = policy.get("weekly_rules", [])
    if not isinstance(rules, list) or len(rules) > 64:
        return "Use at most 64 weekly schedule windows."
    slots: set[int] = set()
    for rule in rules:
        if not isinstance(rule, dict) or rule.get("mode") not in MODES:
            return "Choose a recording mode for every window."
        days = rule.get("days", [])
        if (
            not isinstance(days, list)
            or not days
            or any(type(day) is not int or not 0 <= day <= 6 for day in days)
            or len(set(days)) != len(days)
        ):
            return "Choose one or more unique days for each window."
        times = []
        for key in ("start", "end"):
            value = rule.get(key)
            if (
                not isinstance(value, str)
                or not re.fullmatch(r"[0-2][0-9]:[0-5][0-9]", value)
                or int(value[:2]) > 23
            ):
                return "Use local times in HH:MM format, 00:00–23:59."
            times.append(int(value[:2]) * 60 + int(value[3:]))
        start, end = times
        duration = (end - start) % 1440 or 1440
        for day in days:
            offset = day * 1440 + (0 if end == start else start)
            window = {(offset + minute) % 10080 for minute in range(duration)}
            if slots & window:
                return "Schedule windows overlap. Adjust their days or times."
            slots.update(window)
    return None
