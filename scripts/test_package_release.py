import hashlib
import json
from pathlib import Path
import struct
import subprocess
import tempfile
import unittest
import zipfile

import package_release as package


class PackageReleaseTests(unittest.TestCase):
    def test_source_allowlist_rejects_local_data_and_build_artifacts(self):
        for name in ('validation/report.json', 'target/release/app.exe', 'output/library.json',
                     '.playwright-cli/screen.png', 'src/cache/accounts.json', '../secret.txt', 'scripts/helper.pyc'):
            self.assertFalse(package.allowed_source_path(name), name)
        for name in ('Cargo.lock', 'src/main.ts', 'crates/core/src/lib.rs', '.github/workflows/release.yml',
                     'docs/THIRD-PARTY-NOTICES.md', 'src-tauri/icons/icon.ico'):
            self.assertTrue(package.allowed_source_path(name), name)

    def test_flat_binary_zip_and_source_identity_are_verified(self):
        with tempfile.TemporaryDirectory(prefix='tool-package-test-') as temporary:
            root = Path(temporary)
            self.fixture(root)
            build = root / 'target/release'
            build.mkdir(parents=True)
            binary = self.pe()
            (build / 'steam-tools-next.exe').write_bytes(binary)
            commit = package.git(root, 'rev-parse', 'HEAD')
            info = {'sourceCommit': commit, 'version': '0.3.0', 'platform': 'windows-x64',
                    'exeSha256': hashlib.sha256(binary).hexdigest()}
            (build / 'tool-build.json').write_text(json.dumps(info))
            output = root / 'release-assets'
            manifest = package.package(root, output, 'v0.3.0', build, cargo_licenses=False)
            self.assertEqual(manifest['sourceCommit'], commit)
            self.assertEqual(manifest['platform'], 'windows-x64')
            self.assertEqual({item['name'] for item in manifest['files']}, set(package.FILES))
            archive = output / manifest['asset']['name']
            with zipfile.ZipFile(archive) as zipped:
                self.assertEqual(set(zipped.namelist()), set(package.FILES))
                self.assertTrue(all('/' not in item for item in zipped.namelist()))
            source = output / 'steamtools-next-0.3.0-source.zip'
            with zipfile.ZipFile(source) as zipped:
                self.assertEqual(json.loads(zipped.read('SOURCE_INFO.json'))['sourceCommit'], commit)
                self.assertFalse(any('target/' in item or '.git/' in item for item in zipped.namelist()))
            with self.assertRaises(ValueError):
                package.package(root, output, 'v0.3.0', build, cargo_licenses=False)

    def test_executable_rejects_wrong_architecture_and_dll(self):
        good = self.pe()
        package.validate_pe(good)
        wrong = bytearray(good)
        struct.pack_into('<H', wrong, 132, 0x14c)
        with self.assertRaises(ValueError):
            package.validate_pe(wrong)
        struct.pack_into('<H', wrong, 132, 0x8664)
        struct.pack_into('<H', wrong, 150, 0x2000)
        with self.assertRaises(ValueError):
            package.validate_pe(wrong)

    @staticmethod
    def pe():
        data = bytearray(512)
        data[:2] = b'MZ'
        struct.pack_into('<I', data, 60, 128)
        data[128:132] = b'PE\0\0'
        struct.pack_into('<H', data, 132, 0x8664)
        struct.pack_into('<H', data, 152, 0x20b)
        return data

    @staticmethod
    def fixture(root):
        (root / 'docs').mkdir()
        (root / 'src-tauri').mkdir()
        (root / 'package.json').write_text('{"version":"0.3.0"}')
        (root / 'src-tauri/tauri.conf.json').write_text('{"version":"0.3.0"}')
        (root / 'LICENSE').write_text('Fixture license')
        (root / 'RELEASE_README.txt').write_text('Fixture usage')
        (root / 'docs/THIRD-PARTY-NOTICES.md').write_text('Fixture notices')
        (root / '.gitignore').write_text('/target/\n/release-assets/\n/.release-stage-*/\n')
        for args in (('init', '-b', 'main'), ('config', 'user.name', 'Fixture'),
                     ('config', 'user.email', 'fixture@example.invalid'), ('add', '.'), ('commit', '-m', 'fixture')):
            subprocess.run(['git', '-C', str(root), *args], check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)


if __name__ == '__main__':
    unittest.main()
