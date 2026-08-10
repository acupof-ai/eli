"""Multi-tool, multi-state agent eval tests.

These tasks require the agent to chain multiple tools and carry state
between steps. They verify that the tool loop, tape state, and tool
feedback signals work together end-to-end.
"""

import os
import tempfile
import uuid

import pytest
from conftest import run_eli, switch_profile, assert_nonempty


PROVIDER = "openai"


def eli_run(prompt: str, timeout: int = 120, chat_id: str | None = None):
    cid = chat_id or f"eval-{uuid.uuid4().hex[:12]}"
    return run_eli("run", prompt, "--chat-id", cid, timeout=timeout)


def setup_provider():
    switch_profile(PROVIDER)


# ---------------------------------------------------------------------------
# Task 1: File read → compute → file write → file read (verify)
# ---------------------------------------------------------------------------

class TestFilePipeline:
    """Read a file, compute over its contents, write result, read back."""

    def test_sum_numbers_and_write_result(self):
        setup_provider()
        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "numbers.txt")
            dst = os.path.join(d, "sum.txt")
            with open(src, "w") as f:
                f.write("10\n20\n30\n40\n50\n")

            r = eli_run(
                f"1. Use fs.read to read {src}.\n"
                f"2. Sum all the numbers in the file.\n"
                f"3. Use fs.write to write the sum (just the number, nothing else) to {dst}.\n"
                f"4. Use fs.read to read {dst} and tell me what number is in it."
            )
            assert r.ok, f"Failed: {r.stderr}"

            # The agent should have written the sum and read it back.
            with open(dst) as f:
                written = f.read().strip()
            assert written == "150", f"Expected sum 150, got '{written}'"

            # The agent's final answer should mention 150.
            assert "150" in r.full_output, (
                f"Agent should report the sum 150. Got: {r.full_output}"
            )


# ---------------------------------------------------------------------------
# Task 2: decision.set → file write referencing decision → verify
# ---------------------------------------------------------------------------

class TestDecisionAndFile:
    """Set a decision, then write a file that references it."""

    def test_decision_persists_into_file(self):
        setup_provider()
        with tempfile.TemporaryDirectory() as d:
            out = os.path.join(d, "decision_note.txt")

            r = eli_run(
                f"1. Use decision.set to record this decision: 'Use Rust for the backend'.\n"
                f"2. Use fs.write to write a note to {out} that starts with the exact "
                f"decision text you just set.\n"
                f"3. Tell me what you wrote."
            )
            assert r.ok, f"Failed: {r.stderr}"

            with open(out) as f:
                content = f.read()
            assert "Rust" in content, (
                f"Decision text should appear in the file. Got: {content}"
            )


# ---------------------------------------------------------------------------
# Task 3: Multi-file transform (read A → write B → edit B → verify)
# ---------------------------------------------------------------------------

class TestMultiFileTransform:
    """Read file A, transform, write to B, edit B, verify final state."""

    def test_uppercase_transform_and_edit(self):
        setup_provider()
        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "lower.txt")
            dst = os.path.join(d, "upper.txt")
            with open(src, "w") as f:
                f.write("hello world\n")

            r = eli_run(
                f"1. Use fs.read to read {src}.\n"
                f"2. Use fs.write to write the UPPERCASE version of the content to {dst}.\n"
                f"3. Use fs.edit on {dst} to replace 'WORLD' with 'ELI'.\n"
                f"4. Use fs.read to read {dst} and tell me the final content."
            )
            assert r.ok, f"Failed: {r.stderr}"

            with open(dst) as f:
                content = f.read().strip()
            assert content == "HELLO ELI", f"Expected 'HELLO ELI', got '{content}'"


# ---------------------------------------------------------------------------
# Task 4: task.create → file read → task.update (state carried)
# ---------------------------------------------------------------------------

class TestTaskAndFileState:
    """Create a task, read a file, update the task based on file content."""

    def test_task_update_reflects_file_content(self):
        setup_provider()
        with tempfile.TemporaryDirectory() as d:
            src = os.path.join(d, "status.txt")
            with open(src, "w") as f:
                f.write("DONE\n")

            r = eli_run(
                f"1. Use task.create to create a task with kind 'review' and prompt "
                f"'Check status file'.\n"
                f"2. Use fs.read to read {src}.\n"
                f"3. If the file says DONE, use task.update to mark the task as completed. "
                f"Otherwise leave it as is.\n"
                f"4. Tell me the task id and its final status."
            )
            assert r.ok, f"Failed: {r.stderr}"
            # We can't easily assert task state from outside, but the run should
            # succeed and the agent should report a task id.
            assert_nonempty(r.full_output)


# ---------------------------------------------------------------------------
# Task 5: web.fetch → fs.write → fs.read (verify content round-trip)
# ---------------------------------------------------------------------------

class TestWebFetchToFile:
    """Fetch a URL, write to file, read back, verify key content."""

    def test_fetch_and_save(self):
        setup_provider()
        with tempfile.TemporaryDirectory() as d:
            out = os.path.join(d, "fetched.txt")

            r = eli_run(
                f"1. Use web.fetch to get the content of https://example.com.\n"
                f"2. Use fs.write to save the fetched content to {out}.\n"
                f"3. Use fs.read to read {out} and tell me whether it contains "
                f"the word 'Example'."
            )
            assert r.ok, f"Failed: {r.stderr}"

            with open(out) as f:
                content = f.read()
            assert "Example" in content, (
                f"Fetched content should contain 'Example'. Got: {content[:200]}"
            )


# ---------------------------------------------------------------------------
# Task 6: tape.search → decision.set based on search result
# ---------------------------------------------------------------------------

class TestTapeSearchToDecision:
    """Search the tape, then make a decision based on what was found."""

    def test_search_then_decide(self):
        setup_provider()
        # First turn: write something to the tape via a normal run.
        marker = f"eval-marker-{uuid.uuid4().hex[:8]}"
        r1 = eli_run(f"Remember this keyword: {marker}. Just acknowledge.")
        assert r1.ok

        # Second turn: search the tape for the marker, then set a decision.
        # Use the same chat-id so the tape is shared.
        cid = f"eval-tape-{uuid.uuid4().hex[:12]}"
        eli_run(f"Remember this keyword: {marker}.", chat_id=cid)

        r2 = eli_run(
            f"1. Use tape.search to find the keyword '{marker}'.\n"
            f"2. If you find it, use decision.set to record: 'marker found in tape'.\n"
            f"3. Tell me whether you found the marker.",
            chat_id=cid,
        )
        assert r2.ok, f"Failed: {r2.stderr}"
        # The agent should report finding the marker.
        output = r2.full_output.lower()
        assert "found" in output or "yes" in output, (
            f"Agent should confirm finding the marker. Got: {r2.full_output}"
        )
