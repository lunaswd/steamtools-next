"""Guard Windows release steps against masked earlier native-command failures."""
from pathlib import Path
import re
import unittest


def unsafe_native_run_blocks(source: str) -> list[str]:
    lines = source.splitlines()
    unsafe = []
    for index, line in enumerate(lines):
        if not re.match(r"\s*(?:-\s*)?run:\s*[|>]\s*$", line):
            continue
        indent = len(line) - len(line.lstrip())
        block = []
        for candidate in lines[index + 1:]:
            if candidate.strip() and len(candidate) - len(candidate.lstrip()) <= indent:
                break
            if candidate.strip():
                block.append(candidate.strip())
        commands = [i for i, value in enumerate(block)
                    if re.match(r"(?:python|npm(?:\.cmd)?|cargo|git|cmake)\s", value)]
        for command_index in commands[:-1]:
            following = block[command_index + 1] if command_index + 1 < len(block) else ""
            if not ("$LASTEXITCODE" in following and re.search(r"\b(?:exit|throw)\b", following)):
                unsafe.append(block[command_index].split()[0])
    return unsafe


class ReleaseWorkflowTests(unittest.TestCase):
    def test_real_release_workflow_does_not_mask_native_test_failure(self):
        path = Path(__file__).resolve().parents[1] / ".github/workflows/release.yml"
        self.assertEqual(unsafe_native_run_blocks(path.read_text(encoding="utf-8")), [])

    def test_original_combined_validation_block_is_rejected(self):
        source = """steps:
  - run: |
      python -m unittest discover
      npm test
      cargo test --workspace
      cargo clippy --workspace
"""
        self.assertEqual(unsafe_native_run_blocks(source), ["python", "npm", "cargo"])

    def test_explicit_failure_guards_preserve_native_exit_codes(self):
        source = """steps:
  - run: |
      python -m unittest discover
      if ($LASTEXITCODE) { exit $LASTEXITCODE }
      npm test
      if ($LASTEXITCODE -ne 0) { throw 'Frontend test failure' }
      cargo test --workspace
"""
        self.assertEqual(unsafe_native_run_blocks(source), [])


if __name__ == "__main__":
    unittest.main()
