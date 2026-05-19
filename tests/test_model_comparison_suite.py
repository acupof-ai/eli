"""Schema checks for the Eli model comparison benchmark."""

import json
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
CASES = ROOT / "tests/benchmarks/model_comparison_hard_tail_cases.json"
SNAPSHOT = ROOT / "tests/snapshots/model_comparison_latest.json"


def load_json(path: Path):
    return json.loads(path.read_text())


def test_suite_has_ten_hard_tail_cases():
    payload = load_json(CASES)
    cases = payload["cases"]
    assert payload["suite"] == "eli-model-comparison-hard-tail"
    assert payload["selection_policy"]["name"] == "frontier-agent-hard-tail-10pct"
    assert len(cases) == 10
    assert capabilities(cases) == expected_capabilities()
    assert has_multi_turn_case(cases)
    assert has_mac_only_metal_case(cases)


def capabilities(cases):
    return {case["capability"] for case in cases}


def expected_capabilities():
    return {
        "analysis",
        "exploration",
        "frontier_science",
        "memory_handoff",
        "metal_performance",
        "programming",
        "subagent_management",
        "tool_execution",
        "writing",
    }


def has_multi_turn_case(cases):
    return any(len(case.get("turns", [])) >= 11 for case in cases)


def has_mac_only_metal_case(cases):
    prompts = " ".join(case_text(case) for case in cases)
    return "macOS" in prompts and "MLX" in prompts and "Metal" in prompts


def case_text(case):
    return " ".join(case.get("turns", [case.get("prompt", "")]))


def test_suite_scores_to_one_hundred_points():
    cases = load_json(CASES)["cases"]
    assert sum(case["max_points"] for case in cases) == 100
    for case in cases:
        assert sum(check["points"] for check in case["rubric"]) == case["max_points"]


def test_hard_tail_cases_are_not_saturated_smoke_prompts():
    cases = load_json(CASES)["cases"]
    forbidden_ids = {
        "paper_cache_method_review",
        "fixed_time_dst_schedule",
        "issue_gateway_dirty_worktree",
        "code_async_shutdown_patch",
        "provider_session_id_failure_triage",
        "tool_read_next_round_recall",
        "bounded_tool_budget_plan",
    }
    assert not forbidden_ids.intersection(case["id"] for case in cases)


def test_snapshot_matches_comparison_shape_when_present():
    if not SNAPSHOT.exists():
        return
    snapshot = load_json(SNAPSHOT)
    case_ids = {case["id"] for case in load_json(CASES)["cases"]}
    assert snapshot["suite"] == "eli-model-comparison-hard-tail"
    assert snapshot["version"] == 2
    assert set(item["id"] for item in snapshot["cases"]) == case_ids
    assert set(snapshot["models"]) == {"dsv4", "gpt55"}
    assert set(snapshot["scores"]["total"]) == {"dsv4", "gpt55"}
    assert snapshot["scores"]["total"]["dsv4"]["max"] == 100
    assert "api_total" not in snapshot["scores"]
