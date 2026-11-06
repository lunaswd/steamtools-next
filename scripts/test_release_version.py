import json
from pathlib import Path
import subprocess
import tempfile
import unittest

import release_version as release


class VersionTests(unittest.TestCase):
    def test_numeric_versions_remain_monotonic_and_bounded(self):
        self.assertEqual(release.next_version((0, 3, 0), 1, []), (0, 3, 1))
        self.assertEqual(release.next_version((0, 3, 0), 2, ['v0.3.20', 'notes']), (0, 3, 21))
        self.assertEqual(release.next_version((1, 5, 4, 1), 3, ['v1.5.4.8']), (1, 5, 4, 9))
        self.assertEqual(release.increment((0, 3, 65535)), (0, 4, 0))
        with self.assertRaises(ValueError):
            release.increment((65535, 65535, 65535))
        with self.assertRaises(ValueError):
            release.parse_version('0.3.1-beta', 3)

    def test_kernel_tag_sources_are_committed_and_reruns_reuse_them(self):
        with tempfile.TemporaryDirectory(prefix='release-version-test-') as temporary:
            root = Path(temporary)
            (root / 'src').mkdir()
            (root / 'src/CMakeLists.txt').write_text('project(OpenSteamTool VERSION 1.5.4.1 LANGUAGES C CXX)\n')
            self.git(root, 'init', '-b', 'main')
            self.git(root, 'config', 'user.name', 'Fixture')
            self.git(root, 'config', 'user.email', 'fixture@example.invalid')
            self.git(root, 'add', '.')
            self.git(root, 'commit', '-m', 'fixture sources')
            baseline = release.git(root, 'rev-parse', 'HEAD')
            result = release.prepare(root, 'kernel', 1, '12345')
            self.assertEqual(result['version'], '1.5.4.2')
            self.assertNotEqual(result['sourceCommit'], baseline)
            self.assertEqual(release.git(root, 'rev-parse', 'main'), baseline)
            self.assertIn('VERSION 1.5.4.2', (root / 'src/CMakeLists.txt').read_text())
            self.assertEqual(release.git(root, 'status', '--porcelain'), '')
            self.assertEqual(release.git(root, 'rev-parse', 'v1.5.4.2'), result['sourceCommit'])
            self.git(root, 'checkout', 'main')
            rerun = release.prepare(root, 'kernel', 1, '12345')
            self.assertTrue(rerun['reused'])
            self.assertEqual(rerun['sourceCommit'], result['sourceCommit'])

    def test_tool_stamp_updates_all_release_version_fields(self):
        with tempfile.TemporaryDirectory(prefix='tool-version-test-') as temporary:
            root = Path(temporary)
            (root / 'src-tauri').mkdir()
            (root / 'src').mkdir()
            (root / 'Cargo.toml').write_text('[workspace.package]\nversion = "0.3.0"\n')
            (root / 'Cargo.lock').write_text('name = "steam-tools-core"\nversion = "0.3.0"\n\nname = "steam-tools-next"\nversion = "0.3.0"\n')
            (root / 'package.json').write_text('{"version":"0.3.0"}')
            (root / 'package-lock.json').write_text('{"version":"0.3.0","packages":{"":{"version":"0.3.0"}}}')
            (root / 'src-tauri/tauri.conf.json').write_text('{"version":"0.3.0","app":{"windows":[{"label":"main","title":"SteamTools Next v0.3.0"}]}}')
            (root / 'src/api.ts').write_text("const environment={version:'0.3.0'};\n")
            (root / 'src/main.ts').write_text("const environment={version:'0.3.0'};\n")
            (root / 'RELEASE_README.txt').write_text('SteamTools Next v0.3.0 — Windows x64\n', encoding='utf-8')
            paths = release.stamp(root, 'tool', '0.3.4')
            self.assertIn('RELEASE_README.txt', paths)
            self.assertEqual(json.loads((root / 'package-lock.json').read_text())['packages']['']['version'], '0.3.4')
            self.assertIn('v0.3.4', (root / 'src-tauri/tauri.conf.json').read_text())
            self.assertIn('version = "0.3.4"', (root / 'Cargo.toml').read_text())
            self.assertEqual((root / 'Cargo.lock').read_text().count('version = "0.3.4"'), 2)

    @staticmethod
    def git(root, *args):
        subprocess.run(['git', '-C', str(root), *args], check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)


if __name__ == '__main__':
    unittest.main()
