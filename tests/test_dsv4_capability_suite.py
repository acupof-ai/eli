"""Schema checks for the DeepSeek V4 hard-tail capability suite."""

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
    assert payload["selection_policy"]["name"] == "mainstream-hard-tail-10pct"
    assert len(cases) == 10
    assert {case["capability"] for case in cases} == {"code", "planning", "tool"}


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
    assert snapshot["suite"] == "dsv4-hard-tail-capability"
    assert set(item["id"] for item in snapshot["cases"]) == case_ids
    assert snapshot["scores"]["total"]["max"] == 100
