#!/usr/bin/env python3
"""Permit privileged tests only on trusted main code and inert staging edits."""
import argparse
import re
import subprocess


def validate_candidate(candidate, trusted, parents, changed_paths, mode):
    if not all(re.fullmatch(r'[0-9a-f]{40}', value) for value in (candidate, trusted)):
        raise ValueError('Immutable full source SHAs required')
    if candidate == trusted:
        return True
    if mode == 'probe' or parents != [trusted] or not set(changed_paths).issubset({'VERSION', 'CHANGELOG.md'}):
        raise ValueError('Candidate contains unreviewed executable changes or is not a direct staged leaf')
    if not changed_paths:
        raise ValueError('Empty staging child is not eligible')
    return True


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--candidate', required=True)
    parser.add_argument('--trusted', required=True)
    parser.add_argument('--mode', choices=['full', 'probe'], required=True)
    args = parser.parse_args()
    if not re.fullmatch(r'[0-9a-f]{40}', args.candidate):
        raise ValueError('Full source SHA required')
    subprocess.run(['git', 'fetch', '--no-tags', 'origin', args.candidate], check=True)
    parents = subprocess.check_output(['git', 'show', '-s', '--format=%P', args.candidate], text=True).split()
    changed = subprocess.check_output(['git', 'diff', '--name-only', args.trusted, args.candidate], text=True).splitlines()
    validate_candidate(args.candidate, args.trusted, parents, changed, args.mode)
    for line in subprocess.check_output(['git', 'ls-tree', args.candidate, '--', 'VERSION', 'CHANGELOG.md'], text=True).splitlines():
        if not line.startswith('100644 blob '):
            raise ValueError('Staging content must be ordinary non-executable files')
    version = subprocess.check_output(['git', 'show', args.candidate + ':VERSION'], text=True).strip()
    if not re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+', version):
        raise ValueError('Candidate VERSION is not a regular release')
