"""Synthetic public-source release guard regressions; no client data is read."""
import tempfile
import unittest
from pathlib import Path
import audit_public_source as audit


class PublicSourceAuditTests(unittest.TestCase):
    def inspect(self, name, text):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8")
            return audit.inspect_source(root, [name])

    def test_regular_tool_source_is_accepted(self):
        self.assertTrue(self.inspect("src/demo.ts", "export const title='Synthetic example';")["passed"])

    def test_runtime_account_and_game_data_names_are_rejected(self):
        for name in ("config/loginusers.vdf", "src/settings.json", "src/game.acf", "validation/run.json", "target/app.exe",
                     "docs/screenshot.png", "src/game.lua", "scripts/private-library.zip", "src/saved-library.json",
                     "docs/private-report.md", "scripts/package.ps1", "crates/data/game-library.json",
                     "scripts/license-overrides/user-library.json", "src-tauri/permissions/account-data.json"):
            self.assertFalse(audit.allowed_path(name), name)

    def test_only_reviewed_tool_bitmap_icons_are_accepted(self):
        self.assertTrue(audit.allowed_path("src-tauri/icons/icon.png"))
        self.assertTrue(audit.allowed_path("src-tauri/icons/icon.ico"))
        self.assertFalse(audit.allowed_path("src-tauri/icons/account-avatar.png"))

    def test_private_project_path_is_rejected_without_echoing_value(self):
        text = "const path='E:/" + "GAL" + "-CHS/sample';"
        result = self.inspect("src/demo.ts", text)
        self.assertFalse(result["passed"])
        self.assertNotIn(text, str(result["errors"]))

    def test_real_token_is_rejected_without_echoing_value(self):
        token = "ghp_" + "A" * 36
        result = self.inspect("src/demo.ts", "const key='" + token + "';")
        self.assertFalse(result["passed"])
        self.assertNotIn(token, str(result["errors"]))

    def test_explicit_synthetic_protocol_fixtures_are_accepted(self):
        text = "const account='" + str(76561197960265728 + 1) + "';\nconst password='PRIVATE_FIXTURE_AUTH_VALUE';"
        self.assertTrue(self.inspect("src/demo.ts", text)["passed"])

    def test_binary_private_utf16_path_is_rejected_without_echo(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "fixture.exe"
            private = "D:/FixturePrivateHome"
            path.write_bytes(b"MZ" + private.encode("utf-16le"))
            result = audit.inspect_binary(path, [private])
            self.assertFalse(result["passed"])
            self.assertNotIn(private, str(result["errors"]))


if __name__ == "__main__":
    unittest.main()
