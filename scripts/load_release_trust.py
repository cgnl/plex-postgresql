#!/usr/bin/env python3
"""Load operator-reviewed release metadata without changing the tested source."""
import argparse
import hashlib
import json
import os
import re
from pathlib import Path


def load(path, raw, repository, default_branch, expected_hash=None):
    # Configured but malformed values must not fall back to another authority.
    trust = json.loads(raw) if raw else json.loads(Path(path).read_text())
    if not isinstance(trust, dict) or trust.get('enabled') is not True:
        raise ValueError('Release publication is not enabled')
    if trust.get('repository') != repository or trust.get('default_branch') != default_branch:
        raise ValueError('Release configuration repository or branch mismatch')
    if type(trust.get('workflow_id')) is not int or trust['workflow_id'] <= 0:
        raise ValueError('Missing trusted workflow identity')
    if not re.fullmatch('[0-9a-f]{64}', trust.get('policy_hash', '')):
        raise ValueError('Missing policy hash')
    digest = hashlib.sha256(json.dumps(trust, sort_keys=True, separators=(',', ':')).encode()).hexdigest()
    if expected_hash is not None and digest != expected_hash:
        raise ValueError('Release configuration changed between jobs')
    return trust, digest


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--path', default='.github/release-trust.json')
    parser.add_argument('--expected-hash')
    args = parser.parse_args()
    raw = os.environ.get('RELEASE_TRUST_JSON', '')
    if not raw and not Path(args.path).exists():
        raise SystemExit('Release publication is unconfigured; genuine bootstrap evidence is required')
    trust, digest = load(args.path, raw, os.environ['GITHUB_REPOSITORY'],
                         os.environ['DEFAULT_BRANCH'], args.expected_hash)
    Path(args.path).write_text(json.dumps(trust, indent=2) + '\n')
    if 'GITHUB_OUTPUT' in os.environ:
        with open(os.environ['GITHUB_OUTPUT'], 'a') as output:
            output.write('enabled=true\nhash=' + digest + '\n')
    print('Release configuration verified:', digest)
