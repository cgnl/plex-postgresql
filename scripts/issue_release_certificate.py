#!/usr/bin/env python3
"""Issue evidence-bound certificates; issuance itself grants no publication authority."""
import argparse
import hashlib
import json
import os
import shutil
from datetime import datetime, timezone
from pathlib import Path

from publish_certified_release import GitHub
from release_certificate import require, validate_workload, profile_inputs


def certification_profiles(latest, ledger, maximum=2):
    profile_inputs(latest, latest['id'])
    require(ledger.get('schema_version') == 1, 'Invalid historical ledger')
    selected = [latest]
    seen = {latest['plex_version']}
    for profile in ledger['profiles']:
        if profile['plex_version'] in seen:
            continue
        if len(selected) > maximum:
            break
        profile_inputs(profile, profile['id'])
        require(profile.get('availability') == 'validated', 'Historical coverage unavailable')
        require(tuple(map(int, profile['plex_version'].split('-')[0].split('.'))) <
                tuple(map(int, latest['plex_version'].split('-')[0].split('.'))), 'Historical profile is not older')
        selected.append(profile)
        seen.add(profile['plex_version'])
    require(len({profile['id'] for profile in selected}) == len(selected), 'Duplicate profile namespace')
    return selected


def aggregate(certificates):
    require(certificates, 'No certificates')
    result = dict(certificates[0])
    for key in ('candidate_source_sha', 'candidate_tree_sha', 'project_version', 'staged_base_sha', 'policy_hash', 'authorization'):
        require(all(certificate[key] == result[key] for certificate in certificates), 'Certificates describe different frozen leaves')
    require(all(certificate['artifacts'][0]['files'] == result['artifacts'][0]['files'] for certificate in certificates),
            'Profiles tested different release asset bytes')
    for key in ('profile_fingerprints', 'expected_cells', 'observed_cells', 'artifacts', 'tested_image_digests', 'profiles'):
        result[key] = [item for certificate in certificates for item in certificate[key]]
    require(len(result['profile_fingerprints']) == len(set(result['profile_fingerprints'])), 'Duplicate certification profile')
    result['release_notes'] = 'Certified native Linux compatibility across all declared current-leaf profiles. No native macOS certificate exists.'
    return result


def issue(root, profile, candidate_sha, client, run_id, attempt, workflow_revision, policy_hash):
    profile_inputs(profile, profile['id'])
    root = Path(root)
    prefix = profile['id'] + '-'
    bundle_root = root / (prefix + 'linux-release-zip')
    bundles = list(bundle_root.glob('*.zip'))
    require(len(bundles) == 1, 'Missing final Linux bundle')
    bundle_hash = hashlib.sha256(bundles[0].read_bytes()).hexdigest()
    cells, images, groups = [], [], {'linuxserver': {}, 'plexinc': {}}
    for variant in ('linuxserver', 'plexinc'):
        for arch in ('amd64', 'arm64'):
            bundle_result = json.loads((root / f'{prefix}native-bundle-{variant}-{arch}/native-bundle/result.json').read_text())
            for flag in ('native_bundle_install_verified', 'musl_dependency_loading_verified',
                         'server_and_scanner_execution_verified', 'invalid_bundle_preserves_plex',
                         'sqlite_to_postgresql_migration_verified', 'migration_source_hashes_preserved',
                         'plex_dependency_precedence_verified', 'postgresql_functionality_verified'):
                require(bundle_result.get(flag) is True, 'Incomplete ZIP lane')
            require(bundle_result['base_image'] == profile['images'][variant]['ref'] and bundle_result['arch'] == arch,
                    'ZIP platform/input mismatch')
            checksum = (root / f'{prefix}native-bundle-{variant}-{arch}/bundle-sha256.txt').read_text().split()[0]
            require(checksum == bundle_hash, 'Installed ZIP differs from final bundle')
            cells.append(f'{profile["id"]}/zip/{variant}/{arch}')
            for postgres in (15, 18):
                lane = root / f'{prefix}candidate-{variant}-{arch}-pg{postgres}'
                image = (lane / 'candidate-ref.txt').read_text().strip()
                require((lane / 'source-sha.txt').read_text().strip() == candidate_sha, 'Candidate source mismatch')
                require((lane / 'postgres-ref.txt').read_text().strip() == profile['images'][f'postgres{postgres}']['ref'], 'PG substitution')
                require(json.loads((lane / 'payload-binding.json').read_text())['bundle_sha256'] == bundle_hash,
                        'Container is not bound to exact ZIP')
                smoke = json.loads((lane / 'container/result.json').read_text())
                for key, expected in (('source_sha', candidate_sha),
                                      ('postgres_image', profile['images'][f'postgres{postgres}']['ref']),
                                      ('bundle_sha256', bundle_hash)):
                    require(smoke.get(key) == expected, 'Smoke provenance mismatch: ' + key)
                validate_workload(smoke, source=candidate_sha, version=profile['plex_version'], variant=variant, arch=arch, image=image)
                require((lane / 'container/installed-version.txt').read_text().strip() == profile['plex_version'], 'Unmeasured Plex version')
                soak = json.loads((root / f'{prefix}soak-{variant}-{arch}-pg{postgres}/soak/result.json').read_text())
                for key, expected in (('source_sha', candidate_sha),
                                      ('postgres_image', profile['images'][f'postgres{postgres}']['ref']),
                                      ('bundle_sha256', bundle_hash)):
                    require(soak.get(key) == expected, 'Soak provenance mismatch: ' + key)
                validate_workload(soak, source=candidate_sha, version=profile['plex_version'], variant=variant, arch=arch, image=image, soak=True, smoke_result=smoke)
                cells.extend([f'{profile["id"]}/smoke/{variant}/{arch}/pg{postgres}', f'{profile["id"]}/soak/{variant}/{arch}/pg{postgres}'])
                images.append(image)
                if postgres == 15:
                    groups[variant][arch] = image
    source = client.get('commits/' + candidate_sha)
    run = client.get(f'actions/runs/{run_id}/attempts/{attempt}')
    require(run['head_sha'] == workflow_revision and run['event'] == 'workflow_dispatch', 'Untrusted issuance event/revision')
    remote = client.get(f'actions/runs/{run_id}/artifacts?per_page=100')['artifacts']
    matches = [item for item in remote if item['name'] == prefix + 'linux-release-zip' and not item['expired']]
    require(len(matches) == 1, 'Ambiguous final artifact')
    artifact = matches[0]
    require(artifact.get('digest', '').startswith('sha256:'), 'Artifact API omitted SHA256')
    import base64
    version = base64.b64decode(client.get('contents/VERSION?ref=' + candidate_sha)['content']).decode().strip()
    main = client.get('commits/' + run['head_branch'])
    require(len(source.get('parents', [])) == 1, 'Staged leaf must have one frozen base')
    staged_base = source['parents'][0]['sha']
    require(main['sha'] in (staged_base, candidate_sha), 'Concurrent main change requires new staging')
    return {'schema_version': 1, 'mode': 'full', 'candidate_source_sha': candidate_sha,
            'candidate_tree_sha': source['commit']['tree']['sha'], 'staged_base_sha': staged_base,
            'project_version': version, 'policy_hash': policy_hash, 'profile_fingerprints': [profile['fingerprint']],
            'profiles': [profile],
            'expected_cells': cells, 'observed_cells': cells, 'tested_image_digests': images,
            'promoted_image_groups': groups,
            'artifacts': [{'name': artifact['name'], 'artifact_id': artifact['id'],
                           'sha256': artifact['digest'].removeprefix('sha256:'), 'kind': 'linux-certified-bundle',
                           'files': {path.name: hashlib.sha256(path.read_bytes()).hexdigest() for path in bundle_root.iterdir() if path.is_file()}}],
            'release_notes': f'Certified native Linux compatibility with Plex {profile["plex_version"]}. macOS has no native acceptance certificate.',
            'authorization': {'repository': client.repository, 'workflow_id': run['workflow_id'],
                              'workflow_revision': workflow_revision, 'event': run['event'], 'run_id': run_id,
                              'run_attempt': attempt, 'head_sha': run['head_sha'], 'conclusion': 'success'}}


def bind_publication_artifact(certificate, client, artifact_id, expected_digest, asset_root):
    """Bind freshly uploaded, byte-identical assets without accepting an older attempt."""
    auth = certificate['authorization']
    run = client.get(f"actions/runs/{auth['run_id']}/attempts/{auth['run_attempt']}")
    remote = client.get(f'actions/artifacts/{artifact_id}')
    require(remote.get('id') == artifact_id, 'Publication artifact ID mismatch')
    require(run['head_sha'] == auth['workflow_revision'] and run['event'] == 'workflow_dispatch', 'Untrusted binding run')
    require(remote.get('workflow_run', {}).get('id') == auth['run_id'] and
            remote['workflow_run'].get('head_sha') == auth['workflow_revision'], 'Foreign publication artifact')
    require(remote.get('name') == f"certified-linux-release-zip-{auth['run_attempt']}" and not remote.get('expired'), 'Wrong publication artifact')
    require(remote.get('digest') == 'sha256:' + expected_digest.removeprefix('sha256:'), 'Publication digest mismatch')
    created = datetime.fromisoformat(remote['created_at'].replace('Z', '+00:00'))
    started = datetime.fromisoformat(run['run_started_at'].replace('Z', '+00:00'))
    require(started <= created <= datetime.now(timezone.utc), 'Publication artifact belongs to old attempt')
    files = {path.name: hashlib.sha256(path.read_bytes()).hexdigest() for path in Path(asset_root).iterdir() if path.is_file()}
    require(files and all(item['files'] == files for item in certificate['artifacts']), 'Publication bytes differ from tested bytes')
    result = dict(certificate)
    result['artifacts'] = [{'name': remote['name'], 'artifact_id': remote['id'],
                            'sha256': expected_digest.removeprefix('sha256:'),
                            'kind': 'linux-certified-bundle', 'files': files}]
    return result


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--evidence')
    parser.add_argument('--profile-json')
    parser.add_argument('--source-sha')
    parser.add_argument('--policy')
    parser.add_argument('--draft')
    parser.add_argument('--artifact-id', type=int)
    parser.add_argument('--artifact-digest')
    parser.add_argument('--assets', required=True)
    parser.add_argument('--ledger', default='.github/plex-compatibility-profiles.json')
    parser.add_argument('--output', required=True)
    args = parser.parse_args()
    if args.draft:
        result = bind_publication_artifact(json.loads(Path(args.draft).read_text()),
                                          GitHub(os.environ['GITHUB_REPOSITORY'], os.environ['GH_TOKEN']),
                                          args.artifact_id, args.artifact_digest, args.assets)
        Path(args.output).parent.mkdir(parents=True, exist_ok=True)
        Path(args.output).write_text(json.dumps(result, indent=2) + '\n')
        raise SystemExit(0)
    profiles = certification_profiles(json.loads(args.profile_json), json.loads(Path(args.ledger).read_text()),
                                      json.loads(Path(args.policy).read_text())['maxHistoricalProfiles'])
    result = aggregate([issue(args.evidence, profile, args.source_sha,
                             GitHub(os.environ['GITHUB_REPOSITORY'], os.environ['GH_TOKEN']),
                             int(os.environ['GITHUB_RUN_ID']), int(os.environ['GITHUB_RUN_ATTEMPT']),
                             os.environ['GITHUB_SHA'], hashlib.sha256(Path(args.policy).read_bytes()).hexdigest())
                        for profile in profiles])
    assets = Path(args.assets)
    assets.mkdir(parents=True, exist_ok=False)
    for name in result['artifacts'][0]['files']:
        shutil.copyfile(Path(args.evidence) / (profiles[0]['id'] + '-linux-release-zip') / name, assets / name)
    Path(args.output).parent.mkdir(parents=True, exist_ok=True)
    Path(args.output).write_text(json.dumps(result, indent=2) + '\n')
