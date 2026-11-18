#!/usr/bin/env python3
"""Publish release binaries to the existing cangling-repo layout. No credentials in source."""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import time
import urllib.error
import urllib.parse
import urllib.request

ROOT = '05f8876b41974cfdadfa1f000d77809c'


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


class Drive:
    def __init__(self, url, token, root=ROOT):
        parsed = urllib.parse.urlsplit(url)
        if parsed.scheme != 'https' and not (parsed.scheme == 'http' and parsed.hostname in ('localhost', '127.0.0.1')):
            raise ValueError('Drive URL must use HTTPS')
        if parsed.username or parsed.password or parsed.query or parsed.fragment:
            raise ValueError('Invalid drive URL')
        if not token:
            raise ValueError('WRITE_TOKEN is required')
        self.base, self.token, self.root = url.rstrip('/') + '/api/v1/drive/', token, root
        self.client = urllib.request.build_opener(NoRedirect())

    def request(self, method, path, data=None, content_type='application/json'):
        body = json.dumps(data).encode() if isinstance(data, dict) else data
        request = urllib.request.Request(self.base + path, data=body, method=method,
                                         headers={'X-Drive-Token': self.token, 'Content-Type': content_type})
        with self.client.open(request, timeout=180) as response:
            result = json.load(response)
        if result.get('code') != 200 or result.get('success') is False:
            raise RuntimeError('Drive API rejected ' + method + ' ' + path.split('?')[0])
        return result['data']

    def entries(self, parent):
        page = 1
        while True:
            result = self.request('GET', 'entries?' + urllib.parse.urlencode({'parentId': parent, 'page': page, 'pageSize': 1000}))
            yield from result['entries']
            if page * result['pageSize'] >= result['total']:
                break
            page += 1

    def directory(self, parent, name):
        entry = next((e for e in self.entries(parent) if e['name'] == name), None)
        if entry is None:
            try:
                entry = self.request('POST', 'directories', {'parentId': parent, 'name': name})
            except urllib.error.HTTPError as error:
                if error.code != 409:
                    raise
                entry = next(e for e in self.entries(parent) if e['name'] == name)
        if not entry['directory']:
            raise RuntimeError('Destination is not a directory: ' + name)
        return entry['id']

    def upload(self, relative, data):
        path = PurePosixPath(relative)
        if path.is_absolute() or '..' in path.parts or not path.parts:
            raise ValueError('Invalid release path')
        parent = self.root
        for component in path.parts[:-1]:
            parent = self.directory(parent, component)
        digest = hashlib.sha256(data).hexdigest()
        # Existing API has no overwrite operation. Never delete directories or unrelated names.
        # Reconcile after uncertain responses so rerunning a workflow is idempotent.
        for attempt in range(3):
            try:
                existing = next((e for e in self.entries(parent) if e['name'] == path.name), None)
                if existing:
                    if existing['directory']:
                        raise RuntimeError('Refusing to replace a directory: ' + relative)
                    if existing.get('sha256') == digest and existing['size'] == len(data):
                        print('Verified existing ' + relative)
                        return
                    self.request('DELETE', 'entries/' + existing['id'])
                result = self.request('PUT', 'files?' + urllib.parse.urlencode({'parentId': parent, 'name': path.name}), data, 'application/octet-stream')
                if result.get('sha256') != digest or result.get('size') != len(data):
                    raise RuntimeError('Uploaded checksum/size mismatch: ' + relative)
                print('Uploaded and verified ' + relative)
                return
            except (urllib.error.URLError, TimeoutError, OSError):
                if attempt == 2:
                    raise
                time.sleep(attempt + 1)


def release_files(project, version, dist):
    if not version.startswith('v') or any(c not in 'v0123456789.' for c in version):
        raise ValueError('Expected a release tag such as v0.1.216')
    mapping = [
        (project + '-linux-amd64', 'binaries/linux/amd64/' + project),
        (project + '-linux-arm64', 'binaries/linux/arm64/' + project),
    ]
    if project == 'cangling-keeper':
        mapping.append(('cangling-keeper-windows-x86_64.exe', 'binaries/windows/amd64/cangling-keeper.exe'))
    # Read every build artifact before replacing anything, including the version marker.
    files = [(remote, (Path(dist) / local).read_bytes()) for local, remote in mapping]
    if any(not body for _, body in files):
        raise ValueError('Empty binary artifact')
    if project == 'cangling-keeper':
        sums = ''.join(hashlib.sha256(body).hexdigest() + '  ' + remote.removeprefix('binaries/') + '\n' for remote, body in files)
        files.append(('binaries/KEEPER_SHA256SUMS', sums.encode()))
    marker = 'KEEPER_VERSION' if project == 'cangling-keeper' else 'VERSION'
    files.append(('binaries/' + marker, (version + '\n').encode()))
    return files


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--project', required=True, choices=['cangling-update', 'cangling-keeper'])
    parser.add_argument('--version', required=True)
    parser.add_argument('--dist', default='dist')
    args = parser.parse_args()
    files = release_files(args.project, args.version, args.dist)
    drive = Drive(os.environ.get('CANGLING_DRIVE_URL', 'https://soft.cangling.cn:22002'), os.environ.get('WRITE_TOKEN', ''))
    for remote, body in files:
        drive.upload(remote, body)


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        # Do not dump HTTP headers, response bodies or credential-bearing request objects.
        raise SystemExit('Drive release upload failed: ' + type(error).__name__)
