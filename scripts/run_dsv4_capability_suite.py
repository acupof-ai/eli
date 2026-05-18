#!/usr/bin/env python3
"""Run the DeepSeek V4 hard-tail capability suite and write a score snapshot."""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import urllib.error
import urllib.request
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_CASES = ROOT / "tests/benchmarks/dsv4_hard_tail_cases.json"
DEFAULT_SNAPSHOT = ROOT / "tests/snapshots/dsv4_capability_latest.json"
DEFAULT_MODEL = "deepseek-v4-pro"
DEFAULT_API_BASE = "https://api.deepseek.com/beta"
LOCAL_CHECKS = [
    {
        "id": "local_handoff_overflow_grace",
        "points": 10,
        "command": [
            "cargo",
            "test",
            "-p",
            "eli",
            "test_injected_overflow_error_during_grace_advances_handoff",
        ],
    },
    {
        "id": "local_subagent_tracker_management",
        "points": 10,
        "command": ["cargo", "test", "-p", "eli", "builtin::subagent::tests::tracker_tests"],
    },
]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--cases", type=Path, default=DEFAULT_CASES)
    parser.add_argument("--snapshot", type=Path, default=DEFAULT_SNAPSHOT)
    parser.add_argument("--model", default=DEFAULT_MODEL)
    parser.add_argument("--api-base", default=DEFAULT_API_BASE)
    parser.add_argument("--timeout", type=int, default=180)
    parser.add_argument("--local-timeout", type=int, default=180)
    parser.add_argument("--no-write-snapshot", action="store_true")
    parser.add_argument("--skip-local-checks", action="store_true")
    parser.add_argument("--fail-on-case-threshold", action="store_true")
    return parser.parse_args()


def load_api_key() -> str:
    return (
        os.environ.get("DEEPSEEK_API_KEY")
        or os.environ.get("ELI_DEEPSEEK_API_KEY")
        or read_eli_key()
        or read_global_deepseek_key()
        or fail("DeepSeek API key not found")
    )


def read_eli_key() -> str | None:
    path = Path(os.environ.get("ELI_HOME", Path.home() / ".eli")) / "auth.json"
    if not path.exists():
        return None
    payload = json.loads(path.read_text())
    return payload.get("deepseek", {}).get("api_key")


def read_global_deepseek_key() -> str | None:
    path = Path.home() / ".deepseek" / "config.toml"
    if not path.exists():
        return None
    match = re.search(r'api_key\s*=\s*"([^"]+)"', path.read_text())
    return match.group(1) if match else None


def fail(message: str) -> str:
    raise SystemExit(message)


def load_cases(path: Path) -> dict[str, Any]:
    payload = json.loads(path.read_text())
    cases = payload.get("cases", [])
    if len(cases) != 10:
        fail(f"expected exactly 10 cases, found {len(cases)}")
    if sum(case.get("max_points", 0) for case in cases) != 100:
        fail("api benchmark cases must sum to 100 points")
    return payload


def call_deepseek(case: dict[str, Any], args: argparse.Namespace, key: str) -> dict[str, Any]:
    body = request_body(case, args.model)
    req = urllib.request.Request(
        f"{args.api_base.rstrip('/')}/chat/completions",
        data=json.dumps(body).encode(),
        headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=args.timeout) as resp:
            return json.loads(resp.read().decode())
    except urllib.error.HTTPError as err:
        detail = err.read().decode(errors="replace")[:600]
        fail(f"{case['id']} HTTP {err.code}: {detail}")


def request_body(case: dict[str, Any], model: str) -> dict[str, Any]:
    body = {
        "model": model,
        "temperature": 0,
        "max_tokens": 4096,
        "messages": [{"role": "user", "content": case["prompt"]}],
    }
    if case["mode"] == "tool_call":
        body["tools"] = case["tools"]
        body["tool_choice"] = "auto"
    return body


def message_from(response: dict[str, Any]) -> dict[str, Any]:
    choices = response.get("choices") or [{}]
    return choices[0].get("message") or {}


def content_text(message: dict[str, Any]) -> str:
    return message.get("content") or ""


def content_json(message: dict[str, Any]) -> Any:
    text = strip_fences(content_text(message)).strip()
    return parse_json(text) or parse_json(extract_json_object(text))


def parse_json(text: str | None) -> Any:
    if not text:
        return None
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        return None


def extract_json_object(text: str) -> str | None:
    start = text.find("{")
    end = text.rfind("}")
    return text[start : end + 1] if start >= 0 and end > start else None


def strip_fences(text: str) -> str:
    match = re.search(r"```(?:json)?\s*(.*?)\s*```", text, re.S)
    return match.group(1) if match else text


def score_case(case: dict[str, Any], message: dict[str, Any]) -> dict[str, Any]:
    details = [score_check(check, message) for check in case["rubric"]]
    score = sum(item["earned"] for item in details)
    return {
        "id": case["id"],
        "capability": case["capability"],
        "score": score,
        "max_points": case["max_points"],
        "passed": score >= case["threshold"],
        "details": details,
        "content_preview": content_text(message)[:800],
        "reasoning_chars": len(message.get("reasoning_content") or ""),
        "tool_calls": compact_tool_calls(message),
    }


def score_check(check: dict[str, Any], message: dict[str, Any]) -> dict[str, Any]:
    kind = check["kind"]
    passed = CHECKS[kind](check, message)
    return {
        "kind": kind,
        "points": check["points"],
        "earned": check["points"] if passed else 0,
        "passed": passed,
    }


def check_json_keys(check: dict[str, Any], message: dict[str, Any]) -> bool:
    payload = content_json(message)
    return isinstance(payload, dict) and all(key in payload for key in check["keys"])


def check_contains_any(check: dict[str, Any], message: dict[str, Any]) -> bool:
    haystack = response_haystack(message)
    return any(needle.lower() in haystack for needle in check["needles"])


def check_contains_all(check: dict[str, Any], message: dict[str, Any]) -> bool:
    haystack = response_haystack(message)
    return all(needle.lower() in haystack for needle in check["needles"])


def check_forbid_all(check: dict[str, Any], message: dict[str, Any]) -> bool:
    haystack = response_haystack(message)
    return all(needle.lower() not in haystack for needle in check["needles"])


def response_haystack(message: dict[str, Any]) -> str:
    return content_text(message).lower()


def check_tool_count_at_least(check: dict[str, Any], message: dict[str, Any]) -> bool:
    return len(tool_calls(message)) >= check["count"]


def check_tool_name(check: dict[str, Any], message: dict[str, Any]) -> bool:
    return any(call_name(call) == check["name"] for call in tool_calls(message))


def check_tool_arg_contains(check: dict[str, Any], message: dict[str, Any]) -> bool:
    return any(arg_contains(call, check["arg"], check["needles"]) for call in tool_calls(message))


def check_tool_arg_equals(check: dict[str, Any], message: dict[str, Any]) -> bool:
    return any(call_args(call).get(check["arg"]) == check["value"] for call in tool_calls(message))


def check_tool_args_contain_text(check: dict[str, Any], message: dict[str, Any]) -> bool:
    text = json.dumps([call_args(call) for call in tool_calls(message)], ensure_ascii=False)
    return all(needle.lower() in text.lower() for needle in check["needles"])


def check_no_tool_calls(_: dict[str, Any], message: dict[str, Any]) -> bool:
    return not tool_calls(message)


def tool_calls(message: dict[str, Any]) -> list[dict[str, Any]]:
    calls = message.get("tool_calls") or []
    return calls if isinstance(calls, list) else []


def call_name(call: dict[str, Any]) -> str | None:
    return (call.get("function") or {}).get("name") or call.get("name")


def call_args(call: dict[str, Any]) -> dict[str, Any]:
    raw = (call.get("function") or {}).get("arguments") or call.get("arguments") or {}
    if isinstance(raw, dict):
        return raw
    if not isinstance(raw, str) or not raw:
        return {}
    try:
        return json.loads(raw)
    except json.JSONDecodeError:
        return {}


def arg_contains(call: dict[str, Any], arg: str, needles: list[str]) -> bool:
    value = str(call_args(call).get(arg, ""))
    return any(needle.lower() in value.lower() for needle in needles)


def compact_tool_calls(message: dict[str, Any]) -> list[dict[str, Any]]:
    return [{"name": call_name(call), "arguments": call_args(call)} for call in tool_calls(message)]


CHECKS = {
    "json_keys": check_json_keys,
    "contains_any": check_contains_any,
    "contains_all": check_contains_all,
    "forbid_all": check_forbid_all,
    "tool_count_at_least": check_tool_count_at_least,
    "tool_name": check_tool_name,
    "tool_arg_contains": check_tool_arg_contains,
    "tool_arg_equals": check_tool_arg_equals,
    "tool_args_contain_text": check_tool_args_contain_text,
    "no_tool_calls": check_no_tool_calls,
}


def summarize(results: list[dict[str, Any]]) -> dict[str, Any]:
    groups: dict[str, dict[str, int]] = defaultdict(lambda: {"score": 0, "max": 0})
    for item in results:
        groups[item["capability"]]["score"] += item["score"]
        groups[item["capability"]]["max"] += item["max_points"]
    return {name: score_block(block) for name, block in sorted(groups.items())}


def score_block(block: dict[str, int]) -> dict[str, float | int]:
    percent = round(block["score"] * 100 / block["max"], 1) if block["max"] else 0.0
    return {"score": block["score"], "max": block["max"], "percent": percent}


def build_snapshot(
    suite: dict[str, Any],
    args: argparse.Namespace,
    results: list[dict[str, Any]],
    local_results: list[dict[str, Any]],
) -> dict[str, Any]:
    api_total = score_block(score_totals(results))
    local_total = score_block(score_totals(local_results))
    combined = score_block(combined_totals(api_total, local_total))
    return {
        "suite": suite["suite"],
        "version": suite["version"],
        "model": args.model,
        "api_base": args.api_base,
        "run_at": datetime.now(timezone.utc).isoformat(),
        "selection_policy": suite["selection_policy"],
        "scores": {
            "api_total": api_total,
            "local_runtime": local_total,
            "combined": combined,
            "by_capability": summarize(results),
        },
        "cases": results,
        "local_checks": local_results,
    }


def score_totals(results: list[dict[str, Any]]) -> dict[str, int]:
    return {
        "score": sum(result["score"] for result in results),
        "max": sum(result["max_points"] for result in results),
    }


def combined_totals(api: dict[str, Any], local: dict[str, Any]) -> dict[str, int]:
    return {"score": api["score"] + local["score"], "max": api["max"] + local["max"]}


def write_snapshot(path: Path, snapshot: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(snapshot, indent=2, ensure_ascii=False) + "\n")


def print_summary(snapshot: dict[str, Any]) -> None:
    print_score("API", snapshot["scores"]["api_total"])
    print_score("LOCAL", snapshot["scores"]["local_runtime"])
    print_score("COMBINED", snapshot["scores"]["combined"])
    for name, block in snapshot["scores"]["by_capability"].items():
        print(f"{name.upper()} {block['score']}/{block['max']} = {block['percent']}%")


def print_score(label: str, block: dict[str, Any]) -> None:
    print(f"{label} {block['score']}/{block['max']} = {block['percent']}%")


def run_cases(suite: dict[str, Any], args: argparse.Namespace, key: str) -> list[dict[str, Any]]:
    results = []
    for case in suite["cases"]:
        print(f"RUN {case['id']}", flush=True)
        result = score_case(case, message_from(call_deepseek(case, args, key)))
        print(f"{case['id']} {result['score']}/{result['max_points']}", flush=True)
        results.append(result)
    return results


def run_local_checks(args: argparse.Namespace) -> list[dict[str, Any]]:
    if args.skip_local_checks:
        return [skipped_check(check) for check in LOCAL_CHECKS]
    return [run_local_check(check, args.local_timeout) for check in LOCAL_CHECKS]


def skipped_check(check: dict[str, Any]) -> dict[str, Any]:
    skipped = {**check, "points": 0}
    return local_result(skipped, 0, False, "skipped", "")


def run_local_check(check: dict[str, Any], timeout: int) -> dict[str, Any]:
    print(f"LOCAL {check['id']}", flush=True)
    try:
        proc = run_local_command(check, timeout)
    except subprocess.TimeoutExpired as err:
        return local_result(check, 0, False, err.stdout or "", err.stderr or "timeout")
    result = local_result(check, check["points"], proc.returncode == 0, proc.stdout, proc.stderr)
    print(f"{check['id']} {result['score']}/{result['max_points']}", flush=True)
    return result


def run_local_command(check: dict[str, Any], timeout: int) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        check["command"],
        cwd=ROOT,
        text=True,
        capture_output=True,
        timeout=timeout,
        check=False,
    )


def local_result(
    check: dict[str, Any],
    earned: int,
    passed: bool,
    stdout: str,
    stderr: str,
) -> dict[str, Any]:
    return {
        "id": check["id"],
        "capability": "local_runtime",
        "score": earned if passed else 0,
        "max_points": check["points"],
        "passed": passed,
        "command": check["command"],
        "stdout_tail": stdout[-1200:],
        "stderr_tail": stderr[-1200:],
    }


def main() -> int:
    args = parse_args()
    suite = load_cases(args.cases)
    key = load_api_key()
    results = run_cases(suite, args, key)
    local_results = run_local_checks(args)
    snapshot = build_snapshot(suite, args, results, local_results)
    if not args.no_write_snapshot:
        write_snapshot(args.snapshot, snapshot)
    print_summary(snapshot)
    if args.fail_on_case_threshold:
        return 0 if all(item["passed"] for item in results) else 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
