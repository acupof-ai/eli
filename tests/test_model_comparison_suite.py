"""Schema checks for the Eli model comparison benchmark."""

import json
import importlib.util
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
CASES = ROOT / "tests/benchmarks/model_comparison_hard_tail_cases.json"
SNAPSHOT = ROOT / "tests/snapshots/model_comparison_latest.json"
RUNNER = ROOT / "scripts/run_model_comparison_suite.py"


def load_json(path: Path):
    return json.loads(path.read_text())


def load_runner():
    spec = importlib.util.spec_from_file_location("model_comparison_runner", RUNNER)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


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


def test_suite_uses_known_check_kinds():
    runner = load_runner()
    cases = load_json(CASES)["cases"]
    kinds = {check["kind"] for case in cases for check in case["rubric"]}
    assert kinds <= set(runner.CHECKS)


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
    assert snapshot["run_environment"].get("output_compat", True) is True
    assert "api_total" not in snapshot["scores"]


def test_snapshot_keeps_dsv4_above_target_when_present():
    if not SNAPSHOT.exists():
        return
    snapshot = load_json(SNAPSHOT)
    assert snapshot["scores"]["total"]["dsv4"]["percent"] >= 95.0


def test_runner_compat_matches_decimal_percentages():
    runner = load_runner()
    assert runner.contains_all("DSv4 = 90.0%; GPT-5.5 = 93.00%", ["90%", "93%"])
    assert runner.contains_any("total scores mask the hard-tail gap", ["masked"])
    assert runner.contains_any("Expand (1−x)⁶³ and integrate", ["(1-x)^63"])


def test_runner_detects_empty_and_truncated_json():
    runner = load_runner()
    assert runner.output_status("(model returned empty response)") == "empty_response"
    assert runner.output_status('{"a": {"b": 1') == "truncated_json"


def test_runner_parses_last_complete_json_after_truncated_prefix():
    runner = load_runner()
    text = '{"a": {"broken": 1\n{"ok": true, "nested": {"b": 2}}'
    assert runner.content_json(text) == {"ok": True, "nested": {"b": 2}}


def test_runner_prefers_json_object_with_required_keys():
    runner = load_runner()
    text = '{"outer": {"partial": 1}\n{"answer": true, "evidence": {"nested": 1}}'
    assert runner.content_json(text, ["answer", "evidence"]) == {
        "answer": True,
        "evidence": {"nested": 1},
    }


def test_runner_repairs_malformed_json_contract():
    runner = load_runner()
    case = {"rubric": [{"kind": "stdout_json_keys", "keys": ["answer"]}]}
    result = {"diagnostics": {"status": "truncated_json"}, "details": []}
    assert runner.needs_output_repair(case, result)
    assert "answer" in runner.repair_prompt(case)


def test_runner_json_field_checks_ignore_other_fields():
    runner = load_runner()
    evidence = {
        "stdout": '{"memo": "keep it", "cuts_made": "removed great"}',
        "last_stdout": '{"memo": "keep it", "cuts_made": "removed great"}',
        "assistant_text": "",
        "output_compat": True,
    }
    check = {"field": "memo", "needles": ["great"]}
    assert runner.check_json_field_forbid_all(check, evidence)


def test_fake_codex_echoes_return_exactly(tmp_path):
    runner = load_runner()
    bin_dir = runner.write_fake_codex(tmp_path)
    codex = bin_dir / "codex"
    prompt = "please return exactly: SUBAGENT_OK hard-tail metal mac-only frontier-science. done"
    out = __import__("subprocess").check_output([str(codex)], input=prompt, text=True)
    assert out.strip() == "SUBAGENT_OK hard-tail metal mac-only frontier-science"


def test_fake_codex_echoes_verbose_return_exactly(tmp_path):
    runner = load_runner()
    bin_dir = runner.write_fake_codex(tmp_path)
    codex = bin_dir / "codex"
    prompt = "Return exactly this string and nothing else: SUBAGENT_OK hard-tail metal mac-only frontier-science"
    out = __import__("subprocess").check_output([str(codex)], input=prompt, text=True)
    assert out.strip() == "SUBAGENT_OK hard-tail metal mac-only frontier-science"
