"""Schema checks for the DeepSeek V4 Eli E2E benchmark."""

import json
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
CASES = ROOT / "tests/benchmarks/dsv4_hard_tail_cases.json"
SNAPSHOT = ROOT / "tests/snapshots/dsv4_capability_latest.json"


def load_json(path: Path):
    return json.loads(path.read_text())


def test_dsv4_suite_has_ten_hard_tail_cases():
    payload = load_json(CASES)
    cases = payload["cases"]
    assert payload["suite"] == "dsv4-eli-e2e-benchmark"
    assert payload["selection_policy"]["name"] == "mainstream-realworld-hard-tail-10pct"
    assert len(cases) == 10
    assert capabilities(cases) == {
        "code_analysis",
        "config_resolution",
        "issue_resolution",
        "memory_handoff",
        "research_planning",
        "subagent_management",
        "time_planning",
        "tool_execution",
    }
    assert drivers(cases) == {"eli_cli", "eli_multi_turn", "eli_run"}


def capabilities(cases):
    return {case["capability"] for case in cases}


def drivers(cases):
    return {case["driver"] for case in cases}


def test_dsv4_suite_scores_to_one_hundred_points():
    cases = load_json(CASES)["cases"]
    assert sum(case["max_points"] for case in cases) == 100
    for case in cases:
        assert sum(check["points"] for check in case["rubric"]) == case["max_points"]


def test_dsv4_snapshot_matches_suite_shape_when_present():
    if not SNAPSHOT.exists():
        return
    snapshot = load_json(SNAPSHOT)
    case_ids = {case["id"] for case in load_json(CASES)["cases"]}
    assert snapshot["suite"] == "dsv4-eli-e2e-benchmark"
    assert set(item["id"] for item in snapshot["cases"]) == case_ids
    assert snapshot["scores"]["eli_e2e_total"]["max"] == 100
    assert "api_total" not in snapshot["scores"]
    assert all("tape" in item for item in snapshot["cases"])
