#!/usr/bin/env python3
"""Classify actual native evidence without treating runner failures as regressions."""
import argparse
import json
import os
from pathlib import Path
from release_certificate import require, profile_inputs


def observe(root, profile, source, mode, jobs_passed):
    root = Path(root)
    profile_inputs(profile, profile['id'])
    prefix = profile['id'] + '-'
    failed, observed, mismatched, measured = [], [], [], []
    for variant in ('linuxserver', 'plexinc'):
        for arch in ('amd64', 'arm64'):
            for postgres in (15, 18):
                name = f'{prefix}candidate-{variant}-{arch}-pg{postgres}'
                lane = root / name
                if not (lane / 'container/result.json').is_file():
                    continue
                require((lane / 'source-sha.txt').read_text().strip() == source, 'Observation source mismatch')
                require((lane / 'postgres-ref.txt').read_text().strip() == profile['images'][f'postgres{postgres}']['ref'], 'Observation PG mismatch')
                result = json.loads((lane / 'container/result.json').read_text())
                require(result['variant'] == variant and result['arch'] == arch and result['plex_version'] == profile['plex_version'], 'Observation lane mismatch')
                if (lane / 'container/installed-version.txt').is_file():
                    actual = (lane / 'container/installed-version.txt').read_text().strip()
                    measured.append(name)
                    if actual != profile['plex_version']:
                        mismatched.append({'cell': name, 'actual_version': actual})
                if result.get('exit_code') != 0 or result.get('workload_passed') is not True:
                    failed.append({'cell': name, 'phase': result.get('phase'), 'exit_code': result.get('exit_code')})
                observed.append(name)
    category = 'insufficient-evidence'
    if mismatched:
        category = 'waiting-for-image'
    elif len(observed) == len(measured) == 8 and jobs_passed and not failed:
        category = 'probe-passed' if mode == 'probe' else 'certified'
    elif not observed:
        category = 'infrastructure-failed'
    elif len(observed) == len(measured) == 8 and failed and all(item['phase'] not in ('setup', None) for item in failed):
        category = 'compatibility-assertion-failed'
    return {'schema_version': 1, 'candidate_source_sha': source, 'profile_id': profile['id'],
            'profile_fingerprint': profile['fingerprint'], 'mode': mode, 'category': category,
            'run_id': os.environ.get('GITHUB_RUN_ID', '0'), 'run_attempt': int(os.environ.get('GITHUB_RUN_ATTEMPT', '0')),
            'failed_cells': failed, 'observed_cells': observed, 'version_mismatches': mismatched}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', required=True)
    parser.add_argument('--profile-json', required=True)
    parser.add_argument('--source', required=True)
    parser.add_argument('--mode', choices=['full', 'probe'], required=True)
    parser.add_argument('--jobs-passed', choices=['true', 'false'], required=True)
    parser.add_argument('--output', required=True)
    args = parser.parse_args()
    result = observe(args.root, json.loads(args.profile_json), args.source, args.mode, args.jobs_passed == 'true')
    Path(args.output).parent.mkdir(parents=True, exist_ok=True)
    Path(args.output).write_text(json.dumps(result, indent=2) + '\n')
    with open(os.environ['GITHUB_OUTPUT'], 'a') as output:
        output.write('failure_category=' + result['category'] + '\n')
