#!/usr/bin/env python3
"""Run Eli hard-tail A/B benchmarks across two live model profiles."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import tempfile
import time
from collections import defaultdict
from contextlib import nullcontext
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_CASES = ROOT / "tests/benchmarks/model_comparison_hard_tail_cases.json"
DEFAULT_SNAPSHOT = ROOT / "tests/snapshots/model_comparison_latest.json"
DEEPSEEK_BASE = "https://api.deepseek.com/beta"
DEFAULT_OUTPUT_COMPAT = os.environ.get("ELI_AB_COMPAT", "1") != "0"


@dataclass(frozen=True)
class ModelSpec:
    label: str
    provider: str
    model: str
    api_base: str | None

    def public(self) -> dict[str, Any]:
        return {
            "label": self.label,
            "provider": self.provider,
            "model": self.model,
            "api_base": self.api_base,
        }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--cases", type=Path, default=DEFAULT_CASES)
    parser.add_argument("--snapshot", type=Path, default=DEFAULT_SNAPSHOT)
    parser.add_argument("--a-label", default="dsv4")
    parser.add_argument("--a-provider", default="deepseek")
    parser.add_argument("--a-model", default="deepseek:deepseek-v4-pro")
    parser.add_argument("--a-api-base", default=DEEPSEEK_BASE)
    parser.add_argument("--b-label", default="gpt55")
    parser.add_argument("--b-provider", default="openai")
    parser.add_argument("--b-model", default="openai:gpt-5.5")
    parser.add_argument("--b-api-base")
    parser.add_argument("--case", action="append", default=[])
    parser.add_argument("--eli-bin", type=Path)
    parser.add_argument("--skip-build", action="store_true")
    parser.add_argument("--keep-eli-home", action="store_true")
    parser.add_argument("--timeout", type=int, default=240)
    parser.add_argument("--build-timeout", type=int, default=180)
    parser.add_argument("--no-write-snapshot", action="store_true")
    parser.add_argument("--no-output-compat", action="store_false", dest="output_compat")
    parser.set_defaults(output_compat=DEFAULT_OUTPUT_COMPAT)
    return parser.parse_args()


def model_specs(args: argparse.Namespace) -> list[ModelSpec]:
    return [
        ModelSpec(args.a_label, args.a_provider, args.a_model, args.a_api_base),
        ModelSpec(args.b_label, args.b_provider, args.b_model, args.b_api_base),
    ]


def load_cases(path: Path, selected: list[str]) -> dict[str, Any]:
    payload = json.loads(path.read_text())
    cases = filter_cases(payload["cases"], selected)
    validate_cases(cases)
    return {**payload, "cases": cases}


def filter_cases(cases: list[dict[str, Any]], selected: list[str]) -> list[dict[str, Any]]:
    if not selected:
        return cases
    wanted = set(selected)
    return [case for case in cases if case["id"] in wanted]


def validate_cases(cases: list[dict[str, Any]]) -> None:
    if sum(case["max_points"] for case in cases) == 0:
        fail("benchmark selection has no scoreable cases")
    for case in cases:
        if sum(check["points"] for check in case["rubric"]) != case["max_points"]:
            fail(f"rubric points mismatch for {case['id']}")


def fail(message: str) -> None:
    raise SystemExit(message)


def eli_prefix(args: argparse.Namespace) -> list[str]:
    if args.eli_bin:
        return [str(args.eli_bin)]
    if args.skip_build:
        return [shutil.which("eli") or "eli"]
    build_current_eli(args.build_timeout)
    return [str(ROOT / "target/debug/eli")]


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


def run_root(args: argparse.Namespace, run_id: str) -> tempfile.TemporaryDirectory[str] | Path:
    if not args.keep_eli_home:
        return tempfile.TemporaryDirectory(prefix="eli-model-ab-")
    target = ROOT / ".tmp" / "model-comparison-home" / run_id
    shutil.rmtree(target, ignore_errors=True)
    target.mkdir(parents=True)
    return target


def root_path(root: tempfile.TemporaryDirectory[str] | Path) -> Path:
    return root if isinstance(root, Path) else Path(root.name)


def prepare_homes(root: Path, specs: list[ModelSpec]) -> dict[str, Path]:
    return {spec.label: prepare_home(root / spec.label / ".eli", spec) for spec in specs}


def prepare_home(home: Path, spec: ModelSpec) -> Path:
    home.mkdir(parents=True, exist_ok=True)
    write_config(home, spec)
    write_auth(home, spec)
    return home


def write_config(home: Path, spec: ModelSpec) -> None:
    lines = ['active_profile = "bench"\n', "tool_notices = false\n\n"]
    lines += ["[profiles.bench]\n", f'provider = "{spec.provider}"\n']
    lines += [f'model = "{spec.model}"\n']
    if spec.api_base:
        lines += [f'api_base = "{spec.api_base}"\n']
    (home / "config.toml").write_text("".join(lines))


def write_auth(home: Path, spec: ModelSpec) -> None:
    if normalized_provider(spec.provider) != "deepseek":
        return
    path = home / "auth.json"
    path.write_text(json.dumps({"deepseek": {"api_key": load_deepseek_key()}}, indent=2) + "\n")
    path.chmod(0o600)


def normalized_provider(provider: str) -> str:
    aliases = {"dsv4": "deepseek", "deepseek-v4": "deepseek", "ds-v4": "deepseek"}
    return aliases.get(provider.strip().lower(), provider.strip().lower())


def load_deepseek_key() -> str:
    return (
        os.environ.get("DEEPSEEK_API_KEY")
        or os.environ.get("ELI_DEEPSEEK_API_KEY")
        or read_eli_auth_key(Path.home() / ".eli" / "auth.json", "deepseek")
        or read_deepseek_toml_key()
        or fail("DeepSeek API key not found")
    )


def read_eli_auth_key(path: Path, provider: str) -> str | None:
    if not path.exists():
        return None
    payload = json.loads(path.read_text())
    entry = payload.get(provider, {})
    return entry.get("api_key") if isinstance(entry, dict) else None


def read_deepseek_toml_key() -> str | None:
    path = Path.home() / ".deepseek" / "config.toml"
    if not path.exists():
        return None
    match = re.search(r'api_key\s*=\s*"([^"]+)"', path.read_text())
    return match.group(1) if match else None


def write_fake_codex(root: Path) -> Path:
    bin_dir = root / "bin"
    bin_dir.mkdir(parents=True, exist_ok=True)
    path = bin_dir / "codex"
    path.write_text(fake_codex_script())
    path.chmod(0o755)
    return bin_dir


def fake_codex_script() -> str:
    return """#!/usr/bin/env python3
import re
import sys

prompt = sys.stdin.read()
match = re.search(r"return exactly:\\s*(.+?)(?:\\.|\\n|$)", prompt, re.I | re.S)
fallback = "SUBAGENT_OK provider config DSML parser ownership"
print((match.group(1) if match else fallback).strip())
"""


def bench_env(home: Path, fake_bin: Path) -> dict[str, str]:
    env = clean_env()
    env.update(
        {
            "ELI_HOME": str(home),
            "ELI_EVOLUTION_DISABLED": "1",
            "ELI_MAX_STEPS": "8",
            "ELI_MAX_TOKENS": "1400",
            "ELI_MODEL_TIMEOUT_SECONDS": "180",
            "RUST_LOG": "error",
            "PATH": f"{fake_bin}{os.pathsep}{env.get('PATH', '')}",
        }
    )
    return env


def clean_env() -> dict[str, str]:
    env = dict(os.environ)
    for key in ["ELI_MODEL", "ELI_API_KEY", "ELI_API_BASE", "ELI_OPENAI_API_KEY"]:
        env.pop(key, None)
    return env


def run_suite(args: argparse.Namespace, root: Path, run_id: str) -> dict[str, Any]:
    suite = load_cases(args.cases, args.case)
    specs = model_specs(args)
    prefix = eli_prefix(args)
    fake_bin = write_fake_codex(root)
    homes = prepare_homes(root, specs)
    cases = run_cases(suite["cases"], specs, prefix, homes, fake_bin, run_id, args)
    return build_snapshot(suite, specs, prefix, homes, cases, args.output_compat)


def run_cases(
    cases: list[dict[str, Any]],
    specs: list[ModelSpec],
    prefix: list[str],
    homes: dict[str, Path],
    fake_bin: Path,
    run_id: str,
    args: argparse.Namespace,
) -> list[dict[str, Any]]:
    return [run_case_pair(case, specs, prefix, homes, fake_bin, run_id, args) for case in cases]


def run_case_pair(
    case: dict[str, Any],
    specs: list[ModelSpec],
    prefix: list[str],
    homes: dict[str, Path],
    fake_bin: Path,
    run_id: str,
    args: argparse.Namespace,
) -> dict[str, Any]:
    results = {}
    for spec in specs:
        results[spec.label] = run_model_case(case, spec, prefix, homes[spec.label], fake_bin, run_id, args)
    return compare_case(case, specs, results)


def run_model_case(
    case: dict[str, Any],
    spec: ModelSpec,
    prefix: list[str],
    home: Path,
    fake_bin: Path,
    run_id: str,
    args: argparse.Namespace,
) -> dict[str, Any]:
    print(f"RUN {spec.label} {case['id']}", flush=True)
    session = f"eli-ab-{run_id}-{spec.label}-{case['id']}"
    runs = case_runs(case, prefix, bench_env(home, fake_bin), session, args.timeout)
    entries, tape_path = read_tape(home, session)
    result = score_case(case, build_evidence(session, runs, entries, tape_path, args.output_compat))
    print(f"{spec.label} {case['id']} {result['score']}/{result['max_points']}", flush=True)
    return result


def case_runs(
    case: dict[str, Any],
    prefix: list[str],
    env: dict[str, str],
    session: str,
    timeout: int,
) -> list[dict[str, Any]]:
    turns = case.get("turns") or [case["prompt"]]
    return [run_eli_turn(prefix, turn, session, env, timeout) for turn in turns]


def run_eli_turn(
    prefix: list[str],
    prompt: str,
    session: str,
    env: dict[str, str],
    timeout: int,
) -> dict[str, Any]:
    return run_command(prefix + ["run", prompt, "--session-id", session], env, timeout)


def run_command(cmd: list[str], env: dict[str, str], timeout: int) -> dict[str, Any]:
    start = time.monotonic()
    try:
        proc = subprocess.run(cmd, cwd=ROOT, env=env, text=True, capture_output=True, timeout=timeout)
        return command_result(cmd, proc.returncode, proc.stdout, proc.stderr, start)
    except subprocess.TimeoutExpired as err:
        return command_result(cmd, -9, err.stdout or "", err.stderr or "timeout", start)


def command_result(cmd: list[str], code: int, stdout: str | bytes, stderr: str | bytes, start: float) -> dict[str, Any]:
    return {
        "command": compact_command(cmd),
        "returncode": code,
        "stdout": text_output(stdout),
        "stderr": text_output(stderr),
        "elapsed_ms": round((time.monotonic() - start) * 1000),
    }


def compact_command(cmd: list[str]) -> list[str]:
    return [compact_arg(arg) for arg in cmd]


def compact_arg(arg: str) -> str:
    return f"<prompt:{len(arg)} chars>" if len(arg) > 160 else arg


def text_output(value: str | bytes | None) -> str:
    if value is None:
        return ""
    return value.decode(errors="replace") if isinstance(value, bytes) else value


def build_evidence(
    session: str,
    runs: list[dict[str, Any]],
    entries: list[dict[str, Any]],
    tape_path: Path | None,
    output_compat: bool,
) -> dict[str, Any]:
    stdout = "\n".join(run["stdout"] for run in runs)
    last_stdout = runs[-1]["stdout"] if runs else ""
    assistant = assistant_text(entries)
    return {
        "session_id": session,
        "runs": runs,
        "returncodes": [run["returncode"] for run in runs],
        "stdout": stdout,
        "last_stdout": last_stdout,
        "assistant_text": assistant,
        "stderr": "\n".join(run["stderr"] for run in runs),
        "tape_entries": entries,
        "tape_path": str(tape_path) if tape_path else None,
        "output_compat": output_compat,
    }


def assistant_text(entries: list[dict[str, Any]]) -> str:
    return "\n".join(message_content(entry) for entry in assistant_messages(entries))


def assistant_messages(entries: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [entry for entry in kind_entries(entries, "message") if entry["payload"].get("role") == "assistant"]


def message_content(entry: dict[str, Any]) -> str:
    content = entry["payload"].get("content", "")
    return content if isinstance(content, str) else json.dumps(content, ensure_ascii=False)


def score_case(case: dict[str, Any], evidence: dict[str, Any]) -> dict[str, Any]:
    details = [score_check(check, evidence) for check in case["rubric"]]
    score = sum(item["earned"] for item in details)
    return result_payload(case, score, details, evidence)


def score_check(check: dict[str, Any], evidence: dict[str, Any]) -> dict[str, Any]:
    passed = CHECKS[check["kind"]](check, evidence)
    return {
        "kind": check["kind"],
        "points": check["points"],
        "earned": check["points"] if passed else 0,
        "passed": passed,
    }


def result_payload(
    case: dict[str, Any],
    score: int,
    details: list[dict[str, Any]],
    evidence: dict[str, Any],
) -> dict[str, Any]:
    return {
        "id": case["id"],
        "capability": case["capability"],
        "score": score,
        "max_points": case["max_points"],
        "passed": score >= case["threshold"],
        "details": details,
        "session_id": evidence["session_id"],
        "runs": compact_runs(evidence["runs"]),
        "stdout_tail": redact_secrets(evidence["stdout"][-1800:]),
        "stderr_tail": redact_secrets(evidence["stderr"][-1200:]),
        "diagnostics": output_diagnostics(evidence),
        "tape": tape_summary(evidence["tape_entries"], evidence["tape_path"]),
    }


def output_diagnostics(evidence: dict[str, Any]) -> dict[str, Any]:
    text = evidence["assistant_text"] or evidence["last_stdout"]
    return {"status": output_status(text), "compat": evidence["output_compat"]}


def output_status(text: str) -> str:
    stripped = text.strip()
    if not stripped or stripped == "(model returned empty response)":
        return "empty_response"
    if looks_like_json(stripped) and content_json(stripped) is None:
        return "truncated_json" if likely_truncated_json(stripped) else "invalid_json"
    return "ok"


def looks_like_json(text: str) -> bool:
    return text.startswith("{") or text.startswith("```")


def likely_truncated_json(text: str) -> bool:
    return text.count("{") > text.count("}") or text.count("[") > text.count("]")


def compact_runs(runs: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [
        {
            "command": run["command"],
            "returncode": run["returncode"],
            "elapsed_ms": run["elapsed_ms"],
            "stdout_tail": redact_secrets(run["stdout"][-900:]),
            "stderr_tail": redact_secrets(run["stderr"][-600:]),
        }
        for run in runs
    ]


def redact_secrets(text: str) -> str:
    text = re.sub(r"(?im)^(\s*[a-z0-9_-]+:\s+)(sk|eyJ|[A-Za-z0-9._-]{8}).+$", r"\1[REDACTED]", text)
    return re.sub(r"(?<![A-Za-z0-9])sk-[A-Za-z0-9._-]{12,}", "sk-[REDACTED]", text)


def compare_case(
    case: dict[str, Any],
    specs: list[ModelSpec],
    results: dict[str, dict[str, Any]],
) -> dict[str, Any]:
    a, b = specs[0].label, specs[1].label
    delta = results[a]["score"] - results[b]["score"]
    return {
        "id": case["id"],
        "capability": case["capability"],
        "max_points": case["max_points"],
        "delta_a_minus_b": delta,
        "winner": winner(delta, a, b),
        "results": results,
    }


def winner(delta: int, a: str, b: str) -> str:
    if delta > 0:
        return a
    if delta < 0:
        return b
    return "tie"


def check_returncode(check: dict[str, Any], evidence: dict[str, Any]) -> bool:
    return all(code == check["value"] for code in evidence["returncodes"])


def check_stdout_json_keys(check: dict[str, Any], evidence: dict[str, Any]) -> bool:
    payload = content_payload(check, evidence)
    return isinstance(payload, dict) and all(key in payload for key in check["keys"])


def check_stdout_contains_all(check: dict[str, Any], evidence: dict[str, Any]) -> bool:
    return contains_all(scoped_text(check, evidence), check["needles"])


def check_stdout_contains_any(check: dict[str, Any], evidence: dict[str, Any]) -> bool:
    return contains_any(scoped_text(check, evidence), check["needles"])


def check_stdout_forbid_all(check: dict[str, Any], evidence: dict[str, Any]) -> bool:
    return forbids_all(scoped_text(check, evidence), check["needles"])


def check_json_field_contains_all(check: dict[str, Any], evidence: dict[str, Any]) -> bool:
    return contains_all(json_field_text(check, evidence), check["needles"])


def check_json_field_contains_any(check: dict[str, Any], evidence: dict[str, Any]) -> bool:
    return contains_any(json_field_text(check, evidence), check["needles"])


def check_json_field_forbid_all(check: dict[str, Any], evidence: dict[str, Any]) -> bool:
    return forbids_all(json_field_text(check, evidence), check["needles"])


def check_no_tool_calls(_: dict[str, Any], evidence: dict[str, Any]) -> bool:
    return not tool_names(evidence["tape_entries"])


def check_tool_name_at_least(check: dict[str, Any], evidence: dict[str, Any]) -> bool:
    return tool_count(evidence["tape_entries"], check["name"]) >= check["count"]


def check_tool_name_count(check: dict[str, Any], evidence: dict[str, Any]) -> bool:
    return tool_count(evidence["tape_entries"], check["name"]) == check["count"]


def check_forbid_tool_names(check: dict[str, Any], evidence: dict[str, Any]) -> bool:
    forbidden = {normalize_tool_name(name) for name in check["names"]}
    return not forbidden.intersection(normalize_tool_name(name) for name in tool_names(evidence["tape_entries"]))


def check_tool_result_contains(check: dict[str, Any], evidence: dict[str, Any]) -> bool:
    return contains_all(tape_kind_text(evidence["tape_entries"], "tool_result"), check["needles"])


def check_anchor_name(check: dict[str, Any], evidence: dict[str, Any]) -> bool:
    return check["name"] in anchor_names(evidence["tape_entries"])


def check_anchor_state_contains(check: dict[str, Any], evidence: dict[str, Any]) -> bool:
    return contains_all(tape_kind_text(evidence["tape_entries"], "anchor"), check["needles"])


def check_event_count_at_least(check: dict[str, Any], evidence: dict[str, Any]) -> bool:
    return event_count(evidence["tape_entries"], check["name"]) >= check["count"]


def scoped_text(check: dict[str, Any], evidence: dict[str, Any]) -> str:
    scope = check.get("scope", "stdout")
    if scope == "assistant":
        return evidence["assistant_text"]
    if scope == "last_stdout":
        return evidence["last_stdout"]
    if scope == "stderr":
        return evidence["stderr"]
    if scope == "tape":
        return json.dumps(evidence["tape_entries"], ensure_ascii=False)
    return evidence["stdout"]


def content_payload(check: dict[str, Any], evidence: dict[str, Any]) -> Any:
    text = scoped_text(check, evidence)
    return content_json(text) or compat_content_json(evidence, text)


def compat_content_json(evidence: dict[str, Any], text: str) -> Any:
    if not evidence["output_compat"] or text == evidence["assistant_text"]:
        return None
    return content_json(evidence["assistant_text"])


def json_field_text(check: dict[str, Any], evidence: dict[str, Any]) -> str:
    payload = content_payload(check, evidence)
    value = json_field(payload, check["field"]) if isinstance(payload, dict) else ""
    return json.dumps(value, ensure_ascii=False) if isinstance(value, (dict, list)) else str(value)


def json_field(payload: dict[str, Any], field: str) -> Any:
    value: Any = payload
    for part in field.split("."):
        value = value.get(part) if isinstance(value, dict) else None
    return value if value is not None else ""


CHECKS = {
    "returncode": check_returncode,
    "stdout_json_keys": check_stdout_json_keys,
    "stdout_contains_all": check_stdout_contains_all,
    "stdout_contains_any": check_stdout_contains_any,
    "stdout_forbid_all": check_stdout_forbid_all,
    "json_field_contains_all": check_json_field_contains_all,
    "json_field_contains_any": check_json_field_contains_any,
    "json_field_forbid_all": check_json_field_forbid_all,
    "no_tool_calls": check_no_tool_calls,
    "tool_name_at_least": check_tool_name_at_least,
    "tool_name_count": check_tool_name_count,
    "forbid_tool_names": check_forbid_tool_names,
    "tool_result_contains": check_tool_result_contains,
    "anchor_name": check_anchor_name,
    "anchor_state_contains": check_anchor_state_contains,
    "event_count_at_least": check_event_count_at_least,
}


def read_tape(home: Path, session: str) -> tuple[list[dict[str, Any]], Path | None]:
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
        "tool_calls": tool_names(entries),
        "anchors": anchor_names(entries),
    }


def event_names(entries: list[dict[str, Any]]) -> list[str]:
    return [entry["payload"].get("name", "") for entry in kind_entries(entries, "event")]


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


def event_count(entries: list[dict[str, Any]], name: str) -> int:
    return sum(1 for entry in kind_entries(entries, "event") if entry["payload"].get("name") == name)


def tool_count(entries: list[dict[str, Any]], name: str) -> int:
    expected = normalize_tool_name(name)
    return sum(1 for tool in tool_names(entries) if normalize_tool_name(tool) == expected)


def tape_kind_text(entries: list[dict[str, Any]], kind: str) -> str:
    payloads = [entry["payload"] for entry in kind_entries(entries, kind)]
    return json.dumps(payloads, ensure_ascii=False)


def normalize_tool_name(name: str) -> str:
    return name.replace(".", "_")


def contains_any(text: str, needles: list[str]) -> bool:
    normalized = normalize_text(text)
    return any(normalize_text(needle) in normalized for needle in needles)


def contains_all(text: str, needles: list[str]) -> bool:
    normalized = normalize_text(text)
    return all(normalize_text(needle) in normalized for needle in needles)


def forbids_all(text: str, needles: list[str]) -> bool:
    normalized = normalize_text(text)
    return all(normalize_text(needle) not in normalized for needle in needles)


def normalize_text(text: str) -> str:
    lowered = text.lower().replace("_", " ").replace("-", " ")
    lowered = re.sub(r"(\d+)\.0+%", r"\1%", lowered)
    lowered = re.sub(r"mac\s+only", "macos", lowered)
    return re.sub(r"\s+", " ", lowered)


def content_json(text: str) -> Any:
    cleaned = strip_fences(text).strip()
    return parse_json(cleaned) or parse_json_prefix(cleaned) or parse_json(extract_json_object(cleaned))


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


def parse_json_prefix(text: str) -> Any:
    try:
        return json.JSONDecoder().raw_decode(text)[0]
    except json.JSONDecodeError:
        return None


def extract_json_object(text: str) -> str | None:
    start = text.find("{")
    end = text.rfind("}")
    return text[start : end + 1] if start >= 0 and end > start else None


def build_snapshot(
    suite: dict[str, Any],
    specs: list[ModelSpec],
    prefix: list[str],
    homes: dict[str, Path],
    cases: list[dict[str, Any]],
    output_compat: bool,
) -> dict[str, Any]:
    return {
        "suite": suite["suite"],
        "version": suite["version"],
        "run_at": datetime.now(timezone.utc).isoformat(),
        "selection_policy": suite["selection_policy"],
        "models": {spec.label: spec.public() for spec in specs},
        "scores": score_summary(cases, specs),
        "run_environment": run_environment(prefix, homes, output_compat),
        "cases": cases,
    }


def run_environment(prefix: list[str], homes: dict[str, Path], output_compat: bool) -> dict[str, Any]:
    return {
        "workspace": str(ROOT),
        "eli_entrypoint": prefix,
        "eli_homes": {label: str(path) for label, path in homes.items()},
        "output_compat": output_compat,
    }


def score_summary(cases: list[dict[str, Any]], specs: list[ModelSpec]) -> dict[str, Any]:
    labels = [spec.label for spec in specs]
    totals = {label: total_for(cases, label) for label in labels}
    return {
        "total": {label: score_block(totals[label]) for label in labels},
        "delta_a_minus_b": totals[labels[0]]["score"] - totals[labels[1]]["score"],
        "by_capability": capability_summary(cases, labels),
    }


def total_for(cases: list[dict[str, Any]], label: str) -> dict[str, int]:
    return {
        "score": sum(case["results"][label]["score"] for case in cases),
        "max": sum(case["results"][label]["max_points"] for case in cases),
    }


def score_block(block: dict[str, int]) -> dict[str, float | int]:
    percent = round(block["score"] * 100 / block["max"], 1) if block["max"] else 0.0
    return {"score": block["score"], "max": block["max"], "percent": percent}


def capability_summary(cases: list[dict[str, Any]], labels: list[str]) -> dict[str, Any]:
    groups: dict[str, dict[str, dict[str, int]]] = defaultdict(dict)
    for case in cases:
        add_case_capability(groups, case, labels)
    return {name: {label: score_block(block) for label, block in data.items()} for name, data in groups.items()}


def add_case_capability(groups: dict[str, dict[str, dict[str, int]]], case: dict[str, Any], labels: list[str]) -> None:
    cap = groups[case["capability"]]
    for label in labels:
        block = cap.setdefault(label, {"score": 0, "max": 0})
        block["score"] += case["results"][label]["score"]
        block["max"] += case["results"][label]["max_points"]


def remove_private_state(root: Path) -> None:
    for path in root.glob("*/.eli/auth.json"):
        path.unlink(missing_ok=True)


def write_snapshot(path: Path, snapshot: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(snapshot, indent=2, ensure_ascii=False) + "\n")


def print_summary(snapshot: dict[str, Any]) -> None:
    for label, block in snapshot["scores"]["total"].items():
        print(f"{label} {block['score']}/{block['max']} = {block['percent']}%")
    print(f"delta_a_minus_b {snapshot['scores']['delta_a_minus_b']}")


def main() -> int:
    args = parse_args()
    run_id = datetime.now(timezone.utc).strftime("%Y%m%d%H%M%S")
    root = run_root(args, run_id)
    with root_context(root):
        path = root_path(root)
        snapshot = run_suite(args, path, run_id)
        remove_private_state(path)
        if not args.no_write_snapshot:
            write_snapshot(args.snapshot, snapshot)
        print_summary(snapshot)
    return 0


def root_context(root: tempfile.TemporaryDirectory[str] | Path):
    if isinstance(root, tempfile.TemporaryDirectory):
        return root
    return nullcontext(root)


if __name__ == "__main__":
    raise SystemExit(main())
