#!/usr/bin/env python3
"""Run the DSv4 Eli end-to-end benchmark and write a score snapshot."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import tempfile
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_CASES = ROOT / "tests/benchmarks/dsv4_hard_tail_cases.json"
DEFAULT_SNAPSHOT = ROOT / "tests/snapshots/dsv4_capability_latest.json"
DEFAULT_MODEL = "deepseek-v4-pro"
DEFAULT_API_BASE = "https://api.deepseek.com/beta"
DEFAULT_PROVIDER = "dsv4"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--cases", type=Path, default=DEFAULT_CASES)
    parser.add_argument("--snapshot", type=Path, default=DEFAULT_SNAPSHOT)
    parser.add_argument("--model", default=DEFAULT_MODEL)
    parser.add_argument("--api-base", default=DEFAULT_API_BASE)
    parser.add_argument("--provider", default=DEFAULT_PROVIDER)
    parser.add_argument("--eli-bin", type=Path)
    parser.add_argument("--eli-home", type=Path)
    parser.add_argument("--skip-build", action="store_true")
    parser.add_argument("--keep-eli-home", action="store_true")
    parser.add_argument("--timeout", type=int, default=240)
    parser.add_argument("--build-timeout", type=int, default=180)
    parser.add_argument("--no-write-snapshot", action="store_true")
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
        fail("Eli benchmark cases must sum to 100 points")
    return payload


def build_current_eli(timeout: int) -> None:
    print("BUILD eli", flush=True)
    proc = subprocess.run(
        ["cargo", "build", "-p", "eli"],
        cwd=ROOT,
        text=True,
        capture_output=True,
        timeout=timeout,
        check=False,
    )
    if proc.returncode != 0:
        fail((proc.stdout + proc.stderr)[-4000:])


def eli_prefix(args: argparse.Namespace) -> list[str]:
    if args.eli_bin:
        return [str(args.eli_bin)]
    if args.skip_build:
        return [shutil.which("eli") or "eli"]
    build_current_eli(args.build_timeout)
    return [str(ROOT / "target/debug/eli")]


def prepare_home(home: Path, key: str, args: argparse.Namespace) -> None:
    home.mkdir(parents=True, exist_ok=True)
    write_auth(home, key)
    write_config(home, args)


def write_auth(home: Path, key: str) -> None:
    path = home / "auth.json"
    path.write_text(json.dumps({"deepseek": {"api_key": key}}, indent=2) + "\n")
    path.chmod(0o600)


def write_config(home: Path, args: argparse.Namespace) -> None:
    config = (
        'active_profile = "dsv4"\n'
        "tool_notices = false\n\n"
        "[profiles.dsv4]\n"
        f'provider = "{args.provider}"\n'
        f'model = "{args.model}"\n'
        f'api_base = "{args.api_base}"\n'
    )
    (home / "config.toml").write_text(config)


def write_fake_codex(bin_dir: Path) -> None:
    bin_dir.mkdir(parents=True, exist_ok=True)
    path = bin_dir / "codex"
    body = "#!/bin/sh\ncat >/dev/null\n"
    body += "printf '%s\\n' 'SUBAGENT_OK provider config DSML parser ownership'\n"
    path.write_text(body)
    path.chmod(0o755)


def e2e_env(home: Path, fake_bin: Path) -> dict[str, str]:
    env = dict(os.environ)
    env.update(
        {
            "ELI_HOME": str(home),
            "ELI_EVOLUTION_DISABLED": "1",
            "ELI_MODEL_TIMEOUT_SECONDS": "180",
            "ELI_MAX_STEPS": "8",
            "RUST_LOG": env.get("RUST_LOG", "warn"),
            "PATH": f"{fake_bin}{os.pathsep}{env.get('PATH', '')}",
        }
    )
    return env


def run_command(cmd: list[str], env: dict[str, str], timeout: int) -> dict[str, Any]:
    try:
        proc = subprocess.run(
            cmd,
            cwd=ROOT,
            env=env,
            text=True,
            capture_output=True,
            timeout=timeout,
            check=False,
        )
        return command_result(cmd, proc.returncode, proc.stdout, proc.stderr)
    except subprocess.TimeoutExpired as err:
        return command_result(cmd, -9, err.stdout or "", err.stderr or "timeout")


def command_result(cmd: list[str], code: int, stdout: str, stderr: str) -> dict[str, Any]:
    return {
        "command": cmd,
        "returncode": code,
        "stdout": text_output(stdout),
        "stderr": text_output(stderr),
    }


def text_output(value: str | bytes | None) -> str:
    if value is None:
        return ""
    if isinstance(value, bytes):
        return value.decode(errors="replace")
    return value


def run_case(
    case: dict[str, Any],
    prefix: list[str],
    env: dict[str, str],
    home: Path,
    run_id: str,
    timeout: int,
) -> dict[str, Any]:
    print(f"RUN {case['id']}", flush=True)
    session = session_id(case, run_id)
    runs = case_runs(case, prefix, env, session, timeout)
    entries, tape_path = read_tape(home, session)
    evidence = build_evidence(case, session, runs, entries, tape_path)
    result = score_case(case, evidence)
    print(f"{case['id']} {result['score']}/{result['max_points']}", flush=True)
    return result


def session_id(case: dict[str, Any], run_id: str) -> str | None:
    if case["driver"] == "eli_cli":
        return None
    return f"dsv4-e2e-{run_id}-{case['id']}"


def case_runs(
    case: dict[str, Any],
    prefix: list[str],
    env: dict[str, str],
    session: str | None,
    timeout: int,
) -> list[dict[str, Any]]:
    if case["driver"] == "eli_cli":
        return [run_command(prefix + case["args"], env, timeout)]
    if case["driver"] == "eli_multi_turn":
        return [run_eli_turn(prefix, turn, session, env, timeout) for turn in case["turns"]]
    if case["driver"] == "eli_run":
        return [run_eli_turn(prefix, case["prompt"], session, env, timeout)]
    fail(f"unknown driver: {case['driver']}")


def run_eli_turn(
    prefix: list[str],
    prompt: str,
    session: str | None,
    env: dict[str, str],
    timeout: int,
) -> dict[str, Any]:
    cmd = prefix + ["run", prompt]
    if session:
        cmd += ["--session-id", session]
    return run_command(cmd, env, timeout)


def build_evidence(
    case: dict[str, Any],
    session: str | None,
    runs: list[dict[str, Any]],
    entries: list[dict[str, Any]],
    tape_path: Path | None,
) -> dict[str, Any]:
    return {
        "case": case,
        "session_id": session,
        "runs": runs,
        "returncodes": [run["returncode"] for run in runs],
        "stdout": "\n".join(run["stdout"] for run in runs),
        "stderr": "\n".join(run["stderr"] for run in runs),
        "tape_entries": entries,
        "tape_path": str(tape_path) if tape_path else None,
    }


def score_case(case: dict[str, Any], evidence: dict[str, Any]) -> dict[str, Any]:
    details = [score_check(check, evidence) for check in case["rubric"]]
    score = sum(item["earned"] for item in details)
    return {
        "id": case["id"],
        "capability": case["capability"],
        "driver": case["driver"],
        "score": score,
        "max_points": case["max_points"],
        "passed": score >= case["threshold"],
        "details": details,
        "session_id": evidence["session_id"],
        "runs": compact_runs(evidence["runs"]),
        "stdout_tail": redact_secrets(evidence["stdout"][-1600:]),
        "stderr_tail": redact_secrets(evidence["stderr"][-1600:]),
        "tape": tape_summary(evidence["tape_entries"], evidence["tape_path"]),
    }


def score_check(check: dict[str, Any], evidence: dict[str, Any]) -> dict[str, Any]:
    kind = check["kind"]
    passed = CHECKS[kind](check, evidence)
    return {
        "kind": kind,
        "points": check["points"],
        "earned": check["points"] if passed else 0,
        "passed": passed,
    }


def compact_runs(runs: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [
        {
            "command": run["command"],
            "returncode": run["returncode"],
            "stdout_tail": redact_secrets(run["stdout"][-800:]),
            "stderr_tail": redact_secrets(run["stderr"][-800:]),
        }
        for run in runs
    ]


def redact_secrets(text: str) -> str:
    text = re.sub(r"(?im)^(\s*deepseek:\s+).+$", r"\1[REDACTED]", text)
    text = re.sub(r"(?im)^(\s*[A-Z0-9_]*API_KEY=).+$", r"\1[REDACTED]", text)
    return re.sub(r"(?<![A-Za-z0-9])sk-[A-Za-z0-9._-]{12,}", "sk-[REDACTED]", text)


def read_tape(home: Path, session: str | None) -> tuple[list[dict[str, Any]], Path | None]:
    if not session:
        return [], None
    path = home / "tapes" / f"{tape_name(session)}.jsonl"
    if not path.exists():
        return [], path
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()], path


def tape_name(session: str) -> str:
    workspace = os.path.realpath(ROOT)
    return f"{md5_16(workspace)}__{md5_16(session)}"


def md5_16(text: str) -> str:
    try:
        digest = hashlib.md5(text.encode(), usedforsecurity=False)
    except TypeError:
        digest = hashlib.md5(text.encode())
    return digest.hexdigest()[:16]


def tape_summary(entries: list[dict[str, Any]], path: str | None) -> dict[str, Any]:
    return {
        "path": path,
        "entry_count": len(entries),
        "events": event_names(entries),
        "agent_run_statuses": agent_run_statuses(entries),
        "tool_calls": tool_names(entries),
        "anchors": anchor_names(entries),
    }


def event_names(entries: list[dict[str, Any]]) -> list[str]:
    return [entry["payload"].get("name", "") for entry in kind_entries(entries, "event")]


def agent_run_statuses(entries: list[dict[str, Any]]) -> list[str]:
    return [
        entry["payload"].get("data", {}).get("status", "")
        for entry in kind_entries(entries, "event")
        if entry["payload"].get("name") == "agent.run"
    ]


def tool_names(entries: list[dict[str, Any]]) -> list[str]:
    names: list[str] = []
    for entry in kind_entries(entries, "tool_call"):
        names.extend(call_name(call) or "" for call in entry["payload"].get("calls", []))
    return [name for name in names if name]


def anchor_names(entries: list[dict[str, Any]]) -> list[str]:
    return [entry["payload"].get("name", "") for entry in kind_entries(entries, "anchor")]


def kind_entries(entries: list[dict[str, Any]], kind: str) -> list[dict[str, Any]]:
    return [entry for entry in entries if entry.get("kind") == kind]


def call_name(call: dict[str, Any]) -> str | None:
    return (call.get("function") or {}).get("name") or call.get("name")


def check_returncode(check: dict[str, Any], evidence: dict[str, Any]) -> bool:
    return all(code == check["value"] for code in evidence["returncodes"])


def check_stdout_json_keys(check: dict[str, Any], evidence: dict[str, Any]) -> bool:
    payload = content_json(evidence["stdout"])
    return isinstance(payload, dict) and all(key in payload for key in check["keys"])


def check_stdout_contains_any(check: dict[str, Any], evidence: dict[str, Any]) -> bool:
    return contains_any(evidence["stdout"], check["needles"])


def check_stdout_contains_all(check: dict[str, Any], evidence: dict[str, Any]) -> bool:
    return contains_all(evidence["stdout"], check["needles"])


def check_stdout_forbid_all(check: dict[str, Any], evidence: dict[str, Any]) -> bool:
    return all(needle.lower() not in evidence["stdout"].lower() for needle in check["needles"])


def check_tape_event(check: dict[str, Any], evidence: dict[str, Any]) -> bool:
    return event_count(evidence["tape_entries"], check["name"]) >= 1


def check_tape_event_count_at_least(check: dict[str, Any], evidence: dict[str, Any]) -> bool:
    return event_count(evidence["tape_entries"], check["name"]) >= check["count"]


def check_tape_agent_run_ok(_: dict[str, Any], evidence: dict[str, Any]) -> bool:
    return "ok" in agent_run_statuses(evidence["tape_entries"])


def check_tape_tool_name(check: dict[str, Any], evidence: dict[str, Any]) -> bool:
    expected = normalize_tool_name(check["name"])
    return any(normalize_tool_name(name) == expected for name in tool_names(evidence["tape_entries"]))


def check_tape_tool_result_contains(check: dict[str, Any], evidence: dict[str, Any]) -> bool:
    return contains_all(tape_kind_text(evidence["tape_entries"], "tool_result"), check["needles"])


def check_tape_anchor_name(check: dict[str, Any], evidence: dict[str, Any]) -> bool:
    return check["name"] in anchor_names(evidence["tape_entries"])


def check_tape_anchor_state_contains(check: dict[str, Any], evidence: dict[str, Any]) -> bool:
    return contains_all(tape_kind_text(evidence["tape_entries"], "anchor"), check["needles"])


def event_count(entries: list[dict[str, Any]], name: str) -> int:
    return sum(1 for entry in kind_entries(entries, "event") if entry["payload"].get("name") == name)


def tape_kind_text(entries: list[dict[str, Any]], kind: str) -> str:
    return json.dumps([entry["payload"] for entry in kind_entries(entries, kind)], ensure_ascii=False)


def normalize_tool_name(name: str) -> str:
    return name.replace(".", "_")


def contains_any(text: str, needles: list[str]) -> bool:
    lowered = text.lower()
    return any(needle.lower() in lowered for needle in needles)


def contains_all(text: str, needles: list[str]) -> bool:
    lowered = text.lower()
    return all(needle.lower() in lowered for needle in needles)


def content_json(text: str) -> Any:
    cleaned = strip_fences(text).strip()
    return parse_json(cleaned) or parse_json(extract_json_object(cleaned))


def strip_fences(text: str) -> str:
    match = re.search(r"```(?:json)?\s*(.*?)\s*```", text, re.S)
    return match.group(1) if match else text


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


CHECKS = {
    "returncode": check_returncode,
    "stdout_json_keys": check_stdout_json_keys,
    "stdout_contains_any": check_stdout_contains_any,
    "stdout_contains_all": check_stdout_contains_all,
    "stdout_forbid_all": check_stdout_forbid_all,
    "tape_event": check_tape_event,
    "tape_event_count_at_least": check_tape_event_count_at_least,
    "tape_agent_run_ok": check_tape_agent_run_ok,
    "tape_tool_name": check_tape_tool_name,
    "tape_tool_result_contains": check_tape_tool_result_contains,
    "tape_anchor_name": check_tape_anchor_name,
    "tape_anchor_state_contains": check_tape_anchor_state_contains,
}


def score_totals(results: list[dict[str, Any]]) -> dict[str, int]:
    return {
        "score": sum(result["score"] for result in results),
        "max": sum(result["max_points"] for result in results),
    }


def score_block(block: dict[str, int]) -> dict[str, float | int]:
    percent = round(block["score"] * 100 / block["max"], 1) if block["max"] else 0.0
    return {"score": block["score"], "max": block["max"], "percent": percent}


def summarize(results: list[dict[str, Any]]) -> dict[str, Any]:
    groups: dict[str, dict[str, int]] = defaultdict(lambda: {"score": 0, "max": 0})
    for item in results:
        groups[item["capability"]]["score"] += item["score"]
        groups[item["capability"]]["max"] += item["max_points"]
    return {name: score_block(block) for name, block in sorted(groups.items())}


def build_snapshot(
    suite: dict[str, Any],
    args: argparse.Namespace,
    results: list[dict[str, Any]],
    home: Path,
    prefix: list[str],
) -> dict[str, Any]:
    total = score_block(score_totals(results))
    return {
        "suite": suite["suite"],
        "version": suite["version"],
        "model": args.model,
        "provider": args.provider,
        "api_base": args.api_base,
        "run_at": datetime.now(timezone.utc).isoformat(),
        "selection_policy": suite["selection_policy"],
        "scores": {"eli_e2e_total": total, "by_capability": summarize(results)},
        "run_environment": {
            "workspace": str(ROOT),
            "eli_home": str(home),
            "eli_entrypoint": prefix,
        },
        "cases": results,
    }


def write_snapshot(path: Path, snapshot: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(snapshot, indent=2, ensure_ascii=False) + "\n")


def print_summary(snapshot: dict[str, Any]) -> None:
    print_score("ELI_E2E", snapshot["scores"]["eli_e2e_total"])
    for name, block in snapshot["scores"]["by_capability"].items():
        print(f"{name.upper()} {block['score']}/{block['max']} = {block['percent']}%")


def print_score(label: str, block: dict[str, Any]) -> None:
    print(f"{label} {block['score']}/{block['max']} = {block['percent']}%")


def run_suite(args: argparse.Namespace, home: Path, fake_bin: Path) -> dict[str, Any]:
    suite = load_cases(args.cases)
    prefix = eli_prefix(args)
    env = e2e_env(home, fake_bin)
    run_id = datetime.now(timezone.utc).strftime("%Y%m%d%H%M%S")
    results = [run_case(case, prefix, env, home, run_id, args.timeout) for case in suite["cases"]]
    return build_snapshot(suite, args, results, home, prefix)


def main() -> int:
    args = parse_args()
    key = load_api_key()
    with tempfile.TemporaryDirectory(prefix="eli-dsv4-e2e-") as tmp:
        home = args.eli_home or Path(tmp) / ".eli"
        fake_bin = Path(tmp) / "bin"
        prepare_home(home, key, args)
        write_fake_codex(fake_bin)
        snapshot = run_suite(args, home, fake_bin)
        if args.keep_eli_home:
            preserved = preserve_home(home)
            rewrite_snapshot_paths(snapshot, home, preserved)
        if not args.no_write_snapshot:
            write_snapshot(args.snapshot, snapshot)
        print_summary(snapshot)
    if args.fail_on_case_threshold:
        return 0 if all(item["passed"] for item in snapshot["cases"]) else 1
    return 0


def preserve_home(home: Path) -> Path:
    target = ROOT / ".tmp" / "dsv4-eli-e2e-home"
    if target.exists():
        shutil.rmtree(target)
    shutil.copytree(home, target)
    remove_preserved_private_state(target)
    print(f"PRESERVED_ELI_HOME {target}", flush=True)
    return target


def remove_preserved_private_state(target: Path) -> None:
    for name in ["auth.json", "taskboard.db", "taskboard.db-shm", "taskboard.db-wal"]:
        (target / name).unlink(missing_ok=True)


def rewrite_snapshot_paths(snapshot: dict[str, Any], old: Path, new: Path) -> None:
    snapshot["run_environment"]["eli_home"] = str(new)
    snapshot["run_environment"]["temp_eli_home"] = str(old)
    for case in snapshot["cases"]:
        path = case.get("tape", {}).get("path")
        if path:
            case["tape"]["path"] = path.replace(str(old), str(new))


if __name__ == "__main__":
    raise SystemExit(main())
