#!/usr/bin/env python3
"""Prepare regular version/changelog content before freezing a certification leaf."""
import argparse
import re
from pathlib import Path


def allocate(current, tags, project_breaking=False):
    if project_breaking:
        raise ValueError('Project breaking change requires maintainer version policy')
    if not re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+', current):
        raise ValueError('VERSION must be a regular release')
    major, minor, patch = map(int, current.split('.'))
    while 'v' + f'{major}.{minor}.{patch}' in tags:
        patch += 1
    return f'{major}.{minor}.{patch}'


def stage(root, tags, plex_version):
    root = Path(root)
    version = allocate((root / 'VERSION').read_text().strip(), tags)
    if not re.fullmatch(r'[0-9]+(?:\.[0-9]+){3}-[0-9a-f]+', plex_version):
        raise ValueError('Invalid Plex version')
    changelog = root / 'CHANGELOG.md'
    content = changelog.read_text()
    heading = f'## [{version}]'
    if heading not in content:
        entry = f'{heading}\n\n- Stage native compatibility verification for Plex {plex_version}. Certification is pending.\n\n'
        first_section = content.find('\n## ')
        if first_section < 0:
            raise ValueError('Changelog has no release section')
        content = content[:first_section + 1] + entry + content[first_section + 1:]
    (root / 'VERSION').write_text(version + '\n')
    changelog.write_text(content)
    return version


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', default='.')
    parser.add_argument('--tags-file', required=True)
    parser.add_argument('--plex-version', required=True)
    args = parser.parse_args()
    print(stage(args.root, set(Path(args.tags_file).read_text().splitlines()), args.plex_version))
