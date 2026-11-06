import hashlib
import json
from pathlib import Path
import tempfile
import unittest
import urllib.error
import urllib.request

import publish_release as release


class FakeGitHub:
    def __init__(self, commit, tag, names, bad_digest=False):
        self.commit, self.tag, self.names = commit, tag, names
        self.release = {'id': 1, 'draft': True, 'tag_name': tag, 'target_commitish': commit,
                        'assets': [], 'upload_url': 'https://uploads.github.com/assets{?name}',
                        'html_url': 'https://github.com/fixture/tool/releases/tag/' + tag}
        self.patched = False
        self.bad_digest = bad_digest

    def request(self, path, method='GET', value=None):
        if '/commits/' in path:
            return {'sha': self.commit}
        if path.endswith('/releases/latest'):
            return {'tag_name': 'v0.3.9'}
        if '/releases/tags/' in path or path.endswith('/releases/1'):
            if method == 'PATCH':
                self.patched = True
                self.release.update(value)
            return self.release
        raise AssertionError(path)

    def raw(self, url, method, data, content_type):
        from urllib.parse import parse_qs, urlparse
        name = parse_qs(urlparse(url).query)['name'][0]
        checksum = ('0' * 64) if self.bad_digest else hashlib.sha256(data).hexdigest()
        item = {'id': len(self.release['assets']) + 1, 'name': name, 'size': len(data),
                'state': 'uploaded', 'digest': 'sha256:' + checksum}
        self.release['assets'].append(item)
        return json.dumps(item).encode()


class PublishTests(unittest.TestCase):
    def test_old_slow_build_does_not_replace_a_newer_latest(self):
        self.assertFalse(release.should_be_latest('v0.3.1', 'v0.3.9'))
        self.assertTrue(release.should_be_latest('v0.3.10', 'v0.3.9'))
        self.assertTrue(release.should_be_latest('v1.5.4.2', 'v1.5.4.1'))

    def test_credentials_are_not_forwarded_to_release_cdn(self):
        request = urllib.request.Request('https://api.github.com/assets/1', headers={'Authorization': 'Bearer FIXTURE'})
        result = release.ReleaseRedirects().redirect_request(request, None, 302, '', {},
                                                              'https://release-assets.githubusercontent.com/asset')
        self.assertFalse(result.has_header('Authorization'))
        with self.assertRaises(ValueError):
            release.ReleaseRedirects().redirect_request(request, None, 302, '', {}, 'https://example.invalid/asset')

    def test_only_complete_hash_verified_draft_is_published(self):
        with tempfile.TemporaryDirectory(prefix='publish-fixture-') as temporary:
            directory, manifest, names = self.fixture(Path(temporary))
            api = FakeGitHub(manifest['sourceCommit'], manifest['releaseTag'], names)
            result = release.publish('fixture/tool', directory, 'tool', api)
            self.assertTrue(result['published'])
            self.assertFalse(result['madeLatest'])
            self.assertEqual({item['name'] for item in api.release['assets']}, names)
            self.assertTrue(api.patched)

    def test_bad_asset_digest_preserves_unpublished_draft(self):
        with tempfile.TemporaryDirectory(prefix='publish-fixture-') as temporary:
            directory, manifest, names = self.fixture(Path(temporary))
            api = FakeGitHub(manifest['sourceCommit'], manifest['releaseTag'], names, bad_digest=True)
            with self.assertRaises(ValueError):
                release.publish('fixture/tool', directory, 'tool', api)
            self.assertFalse(api.patched)
            self.assertTrue(api.release['draft'])

    @staticmethod
    def fixture(directory):
        version = '0.3.1'
        binary_name = f'steamtools-next-{version}-windows-x64.zip'
        data = b'fixture binary ZIP'
        manifest = {'schemaVersion': 1, 'version': version, 'releaseTag': 'v' + version,
                    'sourceCommit': 'a' * 40, 'platform': 'windows-x64',
                    'asset': {'name': binary_name, 'sizeBytes': len(data), 'sha256': hashlib.sha256(data).hexdigest()}}
        names = {binary_name, f'steamtools-next-{version}-source.zip', 'tool-release.json', 'release-SHA256SUMS.txt'}
        (directory / binary_name).write_bytes(data)
        (directory / f'steamtools-next-{version}-source.zip').write_bytes(b'fixture source ZIP')
        (directory / 'tool-release.json').write_text(json.dumps(manifest))
        (directory / 'release-SHA256SUMS.txt').write_text('fixture checksum list')
        return directory, manifest, names


if __name__ == '__main__':
    unittest.main()
