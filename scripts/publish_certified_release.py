#!/usr/bin/env python3
"""Authenticate existing certified bytes; never execute candidate code or rebuild."""
import argparse
import hashlib
import json
import os
import subprocess
import tempfile
import urllib.error
import urllib.request
import zipfile
import urllib.parse
import sys
from pathlib import Path

from release_certificate import authenticate, require, trusted_file_paths
from promote_certified_images import promotion_commands, promote


class GitHub:
    def __init__(self, repository, token):
        self.repository, self.token = repository, token

    def get(self, path, raw=False):
        request = urllib.request.Request('https://api.github.com/repos/' + self.repository + '/' + path,
                                         headers={'Authorization': 'Bearer ' + self.token,
                                                  'Accept': 'application/vnd.github+json',
                                                  'X-GitHub-Api-Version': '2022-11-28'})
        class NoRedirect(urllib.request.HTTPRedirectHandler):
            def redirect_request(self, req, fp, code, msg, headers, newurl):
                return None
        try:
            with urllib.request.build_opener(NoRedirect).open(request, timeout=60) as response:
                data = response.read()
        except urllib.error.HTTPError as error:
            if not raw or error.code not in (301, 302, 303, 307, 308):
                raise
            location = error.headers['Location']
            require(urllib.parse.urlparse(location).scheme == 'https', 'Unsafe artifact redirect')
            # Signed artifact storage URLs never receive the repository token.
            with urllib.request.urlopen(location, timeout=60) as response:
                data = response.read()
        return data if raw else json.loads(data)

    def post(self, path, body):
        request = urllib.request.Request('https://api.github.com/repos/' + self.repository + '/' + path,
                                         data=json.dumps(body).encode(), method='POST',
                                         headers={'Authorization': 'Bearer ' + self.token,
                                                  'Accept': 'application/vnd.github+json', 'Content-Type': 'application/json'})
        with urllib.request.urlopen(request, timeout=60) as response:
            return json.loads(response.read())


def extract_assets(archive, destination, expected_files):
    """Only declared regular assets, never paths or executable payloads."""
    with zipfile.ZipFile(archive) as bundle:
        require(set(bundle.namelist()) == set(expected_files), 'Unexpected release artifact contents')
        for info in bundle.infolist():
            require(info.filename == Path(info.filename).name and info.file_size <= 512 * 1024 * 1024,
                    'Unsafe release asset')
            require((info.external_attr >> 16) & 0o170000 != 0o120000, 'Symlink release asset')
            data = bundle.read(info)
            require(hashlib.sha256(data).hexdigest() == expected_files[info.filename], 'Release byte substitution')
            (destination / info.filename).write_bytes(data)


def preserved_main(source, main, staged_base, permit_merge):
    direct_merge = (len(main.get('parents', [])) == 2
                    and {parent['sha'] for parent in main['parents']} == {source['sha'], staged_base}
                    and main['commit']['tree']['sha'] == source['commit']['tree']['sha'])
    merge_needed = main['sha'] == staged_base and permit_merge
    require(main['sha'] == source['sha'] or direct_merge or merge_needed,
            'Main changed after staging; restage and recertify')
    require(source.get('parents') == [{'sha': staged_base}] or
            (len(source.get('parents', [])) == 1 and source['parents'][0]['sha'] == staged_base),
            'Candidate does not preserve the staged base')
    return merge_needed


def expected_coverage(profiles, policy_hash):
    cells = []
    for profile in profiles:
        for variant in ('linuxserver', 'plexinc'):
            for arch in ('amd64', 'arm64'):
                cells.append(f'{profile["id"]}/zip/{variant}/{arch}')
                for postgres in (15, 18):
                    for phase in ('smoke', 'soak'):
                        cells.append(f'{profile["id"]}/{phase}/{variant}/{arch}/pg{postgres}')
    return {'profile_fingerprints': [profile['fingerprint'] for profile in profiles],
            'expected_cells': cells, 'policy_hash': policy_hash}


def resolve_current_coverage(root):
    from issue_release_certificate import certification_profiles
    from plex_compatibility import make_profile
    repository_root = Path(__file__).resolve().parents[1]
    snapshot = root / 'independent-current-upstream.json'
    environment = dict(os.environ)
    environment.pop('GITHUB_OUTPUT', None)
    subprocess.run([sys.executable, str(repository_root / 'scripts/check-upstream-updates.py'), '--output', str(snapshot)],
                   cwd=repository_root, check=True, env=environment, stdout=subprocess.DEVNULL)
    policy_path = repository_root / '.github/plex-compatibility-policy.json'
    ledger = json.loads((repository_root / '.github/plex-compatibility-profiles.json').read_text())
    profiles = certification_profiles(make_profile(json.loads(snapshot.read_text())), ledger,
                                      json.loads(policy_path.read_text())['maxHistoricalProfiles'])
    return profiles, expected_coverage(profiles, hashlib.sha256(policy_path.read_bytes()).hexdigest())


def publish(certificate, trust, client, root, execute=False, certificate_run_id=None):
    auth = certificate['authorization']
    require(certificate_run_id is not None and auth['run_id'] == certificate_run_id, 'Certificate came from a different requested run')
    latest_attempt = client.get(f'actions/runs/{certificate_run_id}')
    require(latest_attempt['run_attempt'] == auth['run_attempt'], 'Certificate came from an earlier run attempt')
    run = client.get(f'actions/runs/{auth["run_id"]}/attempts/{auth["run_attempt"]}')
    workflow = client.get(f'actions/workflows/{auth["workflow_id"]}')
    artifacts = client.get(f'actions/runs/{auth["run_id"]}/artifacts?per_page=100')
    source = client.get('commits/' + certificate['candidate_source_sha'])
    reviewed_blobs = None
    if 'reviewed_file_blobs' in trust:
        reviewed_blobs = {path: client.get('contents/' + path + '?ref=' + auth['workflow_revision'])['sha']
                          for path in trusted_file_paths}
    independent = None
    if trust.get('require_current_upstream') is True:
        profiles, independent = resolve_current_coverage(root)
        require([profile['fingerprint'] for profile in certificate['profiles']] == [profile['fingerprint'] for profile in profiles],
                'Certificate does not cover independently resolved latest/history')
        from plex_compatibility import validate_profile
        for profile in certificate['profiles']:
            validate_profile(profile)
    authenticate(certificate, trust, run, workflow, artifacts, source, reviewed_blobs, independent)
    promoted_indexes = []
    if trust.get('promote_images') is True:
        promotion_commands(certificate, client.repository.split('/')[0])
    main = client.get('commits/' + trust['default_branch'])
    comparison = client.get('compare/' + certificate['candidate_source_sha'] + '...' + main['sha'])
    require(comparison['status'] in ('identical', 'ahead', 'behind'), 'Tested leaf diverges from main')
    # Either fast-forward to the tested leaf, or a pure two-parent merge retaining
    # that leaf and its certified base. Unrelated main edits require a new freeze.
    merge_needed = preserved_main(source, main, certificate['staged_base_sha'], trust.get('merge_staged_leaf') is True)
    version_file = client.get('contents/VERSION?ref=' + certificate['candidate_source_sha'])
    import base64
    require(base64.b64decode(version_file['content']).decode().strip() == certificate['project_version'], 'Version leaf changed')
    assets = {}
    for item in certificate['artifacts']:
        data = client.get(f'actions/artifacts/{item["artifact_id"]}/zip', raw=True)
        require(hashlib.sha256(data).hexdigest() == item['sha256'], 'Downloaded artifact digest mismatch')
        archive = root / f'{item["artifact_id"]}.zip'
        archive.write_bytes(data)
        extract_assets(archive, root, item['files'])
        assets.update(item['files'])
    tag = 'v' + certificate['project_version']
    try:
        reference = client.get('git/ref/tags/' + tag)
    except urllib.error.HTTPError as error:
        if error.code != 404:
            raise
        reference = None
    if reference:
        require(reference['object']['type'] == 'commit' and reference['object']['sha'] == source['sha'], 'Immutable tag already differs')
    try:
        release = client.get('releases/tags/' + tag)
    except urllib.error.HTTPError as error:
        if error.code != 404:
            raise
        release = None
    if release:
        require(reference is not None, 'Release exists without immutable tag')
        require(release['prerelease'] is False, 'Existing release is not regular')
        for asset in release['assets']:
            if asset['name'] in assets:
                require(asset.get('digest') == 'sha256:' + assets[asset['name']], 'Existing asset differs')
                del assets[asset['name']]
    if not execute:
        return {'state': 'validated-dry-run', 'tag': tag, 'assets': sorted(assets)}
    if merge_needed:
        current = client.get('commits/' + trust['default_branch'])
        require(current['sha'] == certificate['staged_base_sha'], 'Concurrent main change before publication')
        # Fast-forward only when base remains unchanged. The Git ref API performs
        # a non-force update, retaining the exact tested candidate as main.
        request = urllib.request.Request('https://api.github.com/repos/' + client.repository + '/git/refs/heads/' + trust['default_branch'],
                                         data=json.dumps({'sha': source['sha'], 'force': False}).encode(), method='PATCH',
                                         headers={'Authorization': 'Bearer ' + client.token,
                                                  'Accept': 'application/vnd.github+json', 'Content-Type': 'application/json'})
        with urllib.request.urlopen(request, timeout=60) as response:
            updated = json.load(response)
        require(updated['object']['sha'] == source['sha'], 'Main was not advanced to tested leaf')
    # gh only uploads validated inert bytes. No candidate script ever runs here.
    env = dict(os.environ, GH_TOKEN=client.token)
    if not release:
        subprocess.run(['gh', 'release', 'create', tag, '--repo', client.repository, '--target', source['sha'],
                        '--title', tag, '--notes', certificate['release_notes']], check=True, env=env)
    if assets:
        subprocess.run(['gh', 'release', 'upload', tag, '--repo', client.repository,
                        *[str(root / name) for name in sorted(assets)]], check=True, env=env)
    if trust.get('promote_images') is True:
        commands = promotion_commands(certificate, client.repository.split('/')[0])
        existing = {}
        for command in commands:
            process = subprocess.run(['docker', 'buildx', 'imagetools', 'inspect', '--raw', command[5]],
                                     capture_output=True, text=True)
            if process.returncode == 0:
                existing[command[5]] = json.loads(process.stdout)
            else:
                require('manifest unknown' in process.stderr.lower() or 'not found' in process.stderr.lower(),
                        'Cannot independently resolve existing immutable image tag')
        promoted_indexes = promote(certificate, client.repository.split('/')[0], existing, execute=True)['certified_indexes']
    return {'state': 'published', 'tag': tag, 'certified_image_indexes': promoted_indexes}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--certificate', required=True)
    parser.add_argument('--trust', required=True)
    parser.add_argument('--execute', action='store_true')
    parser.add_argument('--certificate-run-id', required=True, type=int)
    args = parser.parse_args()
    certificate, trust = (json.loads(Path(path).read_text()) for path in (args.certificate, args.trust))
    with tempfile.TemporaryDirectory(prefix='plex-certified-release-') as directory:
        print(json.dumps(publish(certificate, trust, GitHub(trust['repository'], os.environ['GH_TOKEN']),
                                 Path(directory), args.execute, args.certificate_run_id)))
