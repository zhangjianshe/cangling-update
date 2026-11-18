import hashlib
import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('uploader', Path(__file__).with_name('upload-drive-release.py'))
uploader = importlib.util.module_from_spec(spec)
spec.loader.exec_module(uploader)


class MemoryDrive(uploader.Drive):
    def __init__(self):
        super().__init__('https://example.com', 'test-only')
        self.items = {self.root: []}
        self.calls = []
        self.reject = False

    def entries(self, parent):
        return iter(self.items[parent])

    def request(self, method, path, data=None, content_type='application/json'):
        self.calls.append((method, path))
        if method == 'POST':
            entry = {'id': str(len(self.items)), 'name': data['name'], 'directory': True}
            self.items[data['parentId']].append(entry)
            self.items[entry['id']] = []
            return entry
        if method == 'DELETE':
            id = path.split('/')[-1]
            for children in self.items.values():
                children[:] = [entry for entry in children if entry['id'] != id]
            return {}
        if method == 'PUT':
            if self.reject:
                raise RuntimeError('injected upload failure')
            query = uploader.urllib.parse.parse_qs(path.split('?')[1])
            entry = {'id': 'file-' + str(len(self.calls)), 'name': query['name'][0], 'directory': False, 'size': len(data), 'sha256': hashlib.sha256(data).hexdigest()}
            self.items[query['parentId'][0]].append(entry)
            return entry
        raise AssertionError(method)


class ReleaseTests(unittest.TestCase):
    def test_layout_markers_checksums_and_all_artifacts_required(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for name in ['cangling-update-linux-amd64', 'cangling-update-linux-arm64', 'cangling-keeper-linux-amd64', 'cangling-keeper-linux-arm64', 'cangling-keeper-windows-x86_64.exe']:
                (root / name).write_bytes(name.encode())
            files = uploader.release_files('cangling-keeper', 'v1.2.3', root)
            self.assertEqual(files[-1], ('binaries/KEEPER_VERSION', b'v1.2.3\n'))
            self.assertIn('linux/amd64/cangling-keeper', files[-2][1].decode())
            self.assertEqual(files[2][0], 'binaries/windows/amd64/cangling-keeper.exe')
            update = uploader.release_files('cangling-update', 'v1.2.3', root)
            self.assertEqual(update[-1], ('binaries/VERSION', b'v1.2.3\n'))
            (root / 'cangling-update-linux-arm64').unlink()
            with self.assertRaises(FileNotFoundError):
                uploader.release_files('cangling-update', 'v1.2.3', root)

    def test_upload_is_verified_idempotent_and_only_replaces_matching_file(self):
        drive = MemoryDrive()
        path = 'binaries/linux/amd64/cangling-update'
        drive.upload(path, b'old')
        count = len(drive.calls)
        drive.upload(path, b'old')
        self.assertEqual(len(drive.calls), count)
        drive.upload('binaries/linux/amd64/keep-me', b'other')
        drive.upload(path, b'new')
        self.assertEqual(sum(method == 'DELETE' for method, _ in drive.calls), 1)
        self.assertTrue(any(e['name'] == 'keep-me' for group in drive.items.values() for e in group))
        with self.assertRaises(ValueError):
            drive.upload('../escape', b'invalid')

    def test_failure_does_not_publish_version_and_refuses_directory_replacement(self):
        drive = MemoryDrive()
        drive.directory(drive.root, 'binaries')
        drive.reject = True
        with self.assertRaises(RuntimeError):
            for path, body in [('binaries/app', b'binary'), ('binaries/VERSION', b'v1.2.3')]:
                drive.upload(path, body)
        self.assertFalse(any(e['name'] == 'VERSION' for group in drive.items.values() for e in group))
        with self.assertRaises(RuntimeError):
            drive.upload('binaries', b'cannot replace directory')

    def test_no_credential_redirects_or_unsafe_url(self):
        with self.assertRaises(ValueError):
            uploader.Drive('http://example.com', 'key')
        self.assertIsNone(uploader.NoRedirect().redirect_request(None, None, 302, '', {}, 'https://evil.example'))


if __name__ == '__main__':
    unittest.main()
