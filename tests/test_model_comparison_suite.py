"""Schema checks for the Eli model comparison benchmark."""

import json
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
CASES = ROOT / "tests/benchmarks/model_comparison_hard_tail_cases.json"
SNAPSHOT = ROOT / "tests/snapshots/model_comparison_latest.json"


def load_json(path: Path):
    return json.loads(path.read_text())


def test_suite_has_ten_discriminative_cases():
    payload = load_json(CASES)
    cases = payload["cases"]
    assert payload["suite"] == "eli-model-comparison-hard-tail"
    assert payload["selection_policy"]["name"] == "mainstream-realworld-hard-tail-10pct"
    assert len(cases) == 10
    assert capabilities(cases) == expected_capabilities()
    assert has_multi_turn_case(cases)


def capabilities(cases):
    return {case["capability"] for case in cases}


def expected_capabilities():
    return {
        "code_analysis",
        "issue_resolution",
        "memory_handoff",
        "planning",
        "research_planning",
        "subagent_management",
        "time_planning",
        "tool_execution",
    }


def has_multi_turn_case(cases):
    return any(len(case.get("turns", [])) >= 9 for case in cases)


def test_suite_scores_to_one_hundred_points():
    cases = load_json(CASES)["cases"]
    assert sum(case["max_points"] for case in cases) == 100
    for case in cases:
        assert sum(check["points"] for check in case["rubric"]) == case["max_points"]


def test_snapshot_matches_comparison_shape_when_present():
    if not SNAPSHOT.exists():
        return
    snapshot = load_json(SNAPSHOT)
    case_ids = {case["id"] for case in load_json(CASES)["cases"]}
    assert snapshot["suite"] == "eli-model-comparison-hard-tail"
    assert set(item["id"] for item in snapshot["cases"]) == case_ids
    assert set(snapshot["models"]) == {"dsv4", "gpt55"}
    assert set(snapshot["scores"]["total"]) == {"dsv4", "gpt55"}
    assert snapshot["scores"]["total"]["dsv4"]["max"] == 100
    assert "api_total" not in snapshot["scores"]
