#!/usr/bin/env python3
"""Compose immutable release indexes exclusively from certified native digests."""
import json
import subprocess
import re
from release_certificate import require


def promotion_commands(certificate, repository_owner):
    groups = certificate['promoted_image_groups']
    require(set(groups) == {'linuxserver', 'plexinc'}, 'Missing certified variant')
    commands = []
    for variant, arches in groups.items():
        require(set(arches) == {'amd64', 'arm64'}, 'Missing certified native image')
        repository = f'ghcr.io/{repository_owner.lower()}/plex-postgresql-{variant}'
        for image in arches.values():
            require(image in certificate['tested_image_digests'] and image.startswith(repository + '@sha256:'),
                    'Image promotion substitution')
        commands.append(['docker', 'buildx', 'imagetools', 'create', '--tag',
                         repository + ':v' + certificate['project_version'], arches['amd64'], arches['arm64']])
    return commands


def promote(certificate, owner, existing_manifests, execute=False):
    commands = promotion_commands(certificate, owner)
    indexes = []
    for command in commands:
        tag = command[5]
        expected = {image.split('@', 1)[1] for image in command[6:]}
        if tag in existing_manifests:
            actual = {manifest['digest'] for manifest in existing_manifests[tag].get('manifests', [])}
            require(actual == expected, 'Immutable image tag already differs')
            continue
        if execute:
            subprocess.run(command, check=True)
    if execute:
        latest_commands = []
        for command in commands:
            version_tag = command[5]
            raw = subprocess.check_output(['docker', 'buildx', 'imagetools', 'inspect', '--raw', version_tag], text=True)
            actual = {manifest['digest'] for manifest in json.loads(raw).get('manifests', [])}
            require(actual == {image.split('@', 1)[1] for image in command[6:]}, 'Published version index differs from tested candidates')
            description = subprocess.check_output(['docker', 'buildx', 'imagetools', 'inspect', version_tag], text=True)
            digests = re.findall(r'^Digest:\s+(sha256:[0-9a-f]{64})$', description, flags=re.MULTILINE)
            require(len(digests) == 1, 'Cannot resolve exact certified release index')
            latest_commands.append((latest_command(version_tag, digests[0]), digests))
        # Validate every immutable variant before changing any mutable tag.
        for latest, digests in latest_commands:
            subprocess.run(latest, check=True)
            latest_description = subprocess.check_output(['docker', 'buildx', 'imagetools', 'inspect', latest[5]], text=True)
            require(re.findall(r'^Digest:\s+(sha256:[0-9a-f]{64})$', latest_description, flags=re.MULTILINE) == digests,
                    'Latest did not reuse the exact certified version index')
            indexes.append(latest[6])
    return {'commands': commands, 'certified_indexes': indexes, 'executed': execute}


def latest_command(version_tag, index_digest):
    require(re.fullmatch(r'sha256:[0-9a-f]{64}', index_digest) is not None, 'Mutable release index')
    repository = version_tag.rsplit(':', 1)[0]
    require(repository.startswith('ghcr.io/') and ':v' in version_tag, 'Invalid certified version tag')
    return ['docker', 'buildx', 'imagetools', 'create', '--tag', repository + ':latest', repository + '@' + index_digest]
