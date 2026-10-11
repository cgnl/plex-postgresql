#!/usr/bin/env python3
"""Publish this explicitly requested manual release from its tested image bytes."""
import json
import subprocess
import sys
from pathlib import Path

from promote_certified_images import promote, promotion_commands
from release_certificate import require, validate_workload

SOURCE = '5dd3b5161d2d330bc804e6a71034166e2aecbae6'


def release_inputs(root):
    groups = {variant: {} for variant in ('linuxserver', 'plexinc')}
    images = []
    for variant in groups:
        for arch in ('amd64', 'arm64'):
            for pg in (15, 18):
                lane = Path(root) / f'candidate-{variant}-{arch}-pg{pg}'
                require((lane / 'source-sha.txt').read_text().strip() == SOURCE, 'Unexpected tested source')
                image = (lane / 'candidate-ref.txt').read_text().strip()
                result = json.loads((lane / 'container/result.json').read_text())
                validate_workload(result, source=SOURCE, version='1.43.4.10903-e5521bd8c',
                                  variant=variant, arch=arch, image=image)
                images.append(image)
                if pg == 15:
                    groups[variant][arch] = image
    # These are manual release inputs, not an automatic publication certificate.
    return {'project_version': '1.3.22', 'promoted_image_groups': groups,
            'tested_image_digests': images}


def publish(root):
    inputs = release_inputs(root)
    existing = {}
    for command in promotion_commands(inputs, 'cgnl'):
        result = subprocess.run(['docker', 'buildx', 'imagetools', 'inspect', '--raw', command[5]],
                                capture_output=True, text=True)
        if result.returncode == 0:
            existing[command[5]] = json.loads(result.stdout)
        else:
            require('not found' in result.stderr.lower() or 'manifest unknown' in result.stderr.lower(),
                    'Cannot check existing immutable Docker version')
    result = promote(inputs, 'cgnl', existing, execute=True)
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    publish(sys.argv[1])
