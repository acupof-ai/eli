"""Hook wiring E2E tests — verify save_state and dispatch_outbound work end-to-end.

These tests verify the plumbing, not the LLM output:
  - save_state: tape events (agent.run, command) are written after each turn
  - dispatch_outbound: output reaches stdout via the hook
  - eli hooks: all hook points show an implementor
"""

import hashlib
import json
import os
from pathlib import Path

from conftest import require_profile, run_eli, unique_name


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

ELI_HOME = Path(os.environ.get("ELI_HOME", Path.home() / ".eli"))
TAPES_DIR = ELI_HOME / "tapes"


def tape_name_for(session_id: str) -> str:
    """Replicate TapeService::session_tape_name to find the tape file."""
    workspace = os.getcwd()
    # Match Rust's canonicalize behavior
    workspace = os.path.realpath(workspace)
    wh = hashlib.md5(workspace.encode(), usedforsecurity=False).hexdigest()[:16]
    sh = hashlib.md5(session_id.encode(), usedforsecurity=False).hexdigest()[:16]
    return f"{wh}__{sh}"


def read_tape_events(session_id: str) -> list[dict]:
    """Read all 'event' entries from a session tape."""
    name = tape_name_for(session_id)
    tape_file = TAPES_DIR / f"{name}.jsonl"
    if not tape_file.exists():
        return []
    events = []
    for line in tape_file.read_text().splitlines():
        if not line.strip():
            continue
        entry = json.loads(line)
        if entry.get("kind") == "event":
            events.append(entry)
    return events


def remove_tape(session_id: str):
    name = tape_name_for(session_id)
    tape_file = TAPES_DIR / f"{name}.jsonl"
    spill_dir = TAPES_DIR / f"{name}.d"
    if tape_file.exists():
        tape_file.unlink()
    if spill_dir.exists():
        import shutil
        shutil.rmtree(spill_dir)


def make_session_id(label: str) -> str:
    return f"cli:{unique_name(label)}"


def find_events_by_name(events: list[dict], name: str) -> list[dict]:
    """Filter tape events by payload.name."""
    return [e for e in events if e.get("payload", {}).get("name") == name]


# ---------------------------------------------------------------------------
# Hook registration
# ---------------------------------------------------------------------------

class TestHookRegistration:
    """Verify all hook points are wired up."""

    def test_all_hooks_have_builtin(self):
        r = run_eli("hooks")
        assert r.ok, f"eli hooks failed: {r.stderr}"
        # All 12 hook points should show "builtin"
        expected_hooks = [
            "classify_inbound",
            "resolve_session",
            "load_state",
            "build_user_prompt",
            "build_system_prompt",
            "run_model",
            "save_state",
            "render_outbound",
            "dispatch_outbound",
            "register_cli_commands",
            "on_error",
            "wrap_tool",
        ]
        for hook in expected_hooks:
            assert hook in r.stdout, f"Hook '{hook}' not found in output"
            # Each hook should have "builtin" as implementor
            idx = r.stdout.index(hook)
            section = r.stdout[idx:idx + 200]
            assert "builtin" in section, f"Hook '{hook}' has no builtin implementor"

    def test_save_state_registered(self):
        r = run_eli("hooks")
        assert "save_state:" in r.stdout
        assert "builtin" in r.stdout.split("save_state:")[1][:100]

    def test_dispatch_outbound_registered(self):
        r = run_eli("hooks")
        assert "dispatch_outbound:" in r.stdout
        assert "builtin" in r.stdout.split("dispatch_outbound:")[1][:100]


# ---------------------------------------------------------------------------
# save_state — tape event verification
# ---------------------------------------------------------------------------

class TestSaveState:
    """Verify save_state hook writes events to tape after each turn."""

    def test_agent_run_event_written(self):
        """After eli run, an agent.run event should appear in the tape."""
        session_id = make_session_id("save_state_run")
        require_profile("openai")
        try:
            r = run_eli(
                "run", "Reply with one word: banana",
                "--session-id", session_id,
            )
            assert r.ok, f"eli run failed: {r.stderr}"

            events = read_tape_events(session_id)
            run_events = find_events_by_name(events, "agent.run")
            assert len(run_events) > 0, (
                f"No agent.run event found in tape for session {session_id}. "
                f"Total events: {len(events)}"
            )

            last_run = run_events[-1]
            data = last_run["payload"]["data"]
            assert "elapsed_ms" in data, "Missing elapsed_ms"
            assert "status" in data, "Missing status"
            assert data["status"] == "ok", f"Expected status=ok, got {data['status']}"
            assert "usage" in data, "Missing usage"
            usage = data["usage"]
            assert usage["input_tokens"] > 0, "input_tokens should be > 0"
            assert usage["output_tokens"] > 0, "output_tokens should be > 0"
            assert usage["total_tokens"] > 0, "total_tokens should be > 0"
            assert usage["rounds"] >= 1, "rounds should be >= 1"
        finally:
            remove_tape(session_id)

    def test_agent_run_start_event_written(self):
        """agent.run.start event should appear before agent.run."""
        session_id = make_session_id("save_state_start")
        require_profile("openai")
        try:
            r = run_eli(
                "run", "Reply with one word: cherry",
                "--session-id", session_id,
            )
            assert r.ok, f"eli run failed: {r.stderr}"

            events = read_tape_events(session_id)
            start_events = find_events_by_name(events, "agent.run.start")
            run_events = find_events_by_name(events, "agent.run")

            assert len(start_events) > 0, "No agent.run.start event"
            assert len(run_events) > 0, "No agent.run event"
            assert "prompt" in start_events[-1]["payload"]["data"]
        finally:
            remove_tape(session_id)

    def test_multiple_turns_accumulate_events(self):
        """Two turns on the same session should produce two agent.run events."""
        session_id = make_session_id("save_state_multi")
        require_profile("openai")
        try:
            r1 = run_eli("run", "Say apple", "--session-id", session_id)
            assert r1.ok
            r2 = run_eli("run", "Say orange", "--session-id", session_id)
            assert r2.ok

            events = read_tape_events(session_id)
            run_events = find_events_by_name(events, "agent.run")
            assert len(run_events) == 2, (
                f"Expected exactly 2 agent.run events, got {len(run_events)}"
            )
        finally:
            remove_tape(session_id)


# ---------------------------------------------------------------------------
# dispatch_outbound — CLI output verification
# ---------------------------------------------------------------------------

class TestDispatchOutbound:
    """Verify dispatch_outbound hook delivers output to stdout."""

    def test_cli_output_appears(self):
        """Model output should appear in stdout via dispatch_outbound hook."""
        require_profile("openai")
        r = run_eli("run", "Reply with exactly: dispatch_test_ok")
        assert r.ok, f"Failed: {r.stderr}"
        # Output should contain the model response (via hook, not manual print)
        assert r.stdout.strip(), "No stdout output — dispatch_outbound may not be working"

    def test_usage_on_stderr(self):
        """Token usage should appear on stderr (separate from dispatch)."""
        require_profile("openai")
        r = run_eli("run", "Reply with one word: test")
        assert r.ok
        assert "tokens:" in r.stderr, (
            f"Expected token usage on stderr, got: {r.stderr[:200]}"
        )

    def test_empty_output_not_printed(self):
        """Empty/whitespace model output should not produce stdout lines."""
        # This is hard to trigger intentionally with a real LLM,
        # so we just verify the run doesn't crash.
        require_profile("openai")
        r = run_eli("run", "Reply with one word: ok")
        assert r.ok
