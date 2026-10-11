#!/usr/bin/env python3
"""Bind a container's actual shared-library bytes to the tested Linux ZIP."""
import argparse
import hashlib
import json
import zipfile
from pathlib import Path
from release_certificate import require


def verify(bundle, directory, arch):
    machine = {'amd64': 'x86_64', 'arm64': 'aarch64'}[arch]
    bindings = {}
    with zipfile.ZipFile(bundle) as archive:
        names = archive.namelist()
        require(len(names) == len(set(names)), 'Duplicate ZIP entries')
        for name in names:
            if name.startswith(f'libs/{machine}/') and not name.endswith('/'):
                basename = Path(name).name
                require(name == f'libs/{machine}/{basename}', 'Unexpected library path')
                bindings[basename] = hashlib.sha256(archive.read(name)).hexdigest()
        bindings['db_interpose_pg.so'] = hashlib.sha256(archive.read(f'db_interpose_pg-linux-{machine}.so')).hexdigest()
    require('libpq.so.5' in bindings and 'libgcc_s.so.1' in bindings, 'Missing library set')
    actual = {path.name: hashlib.sha256(path.read_bytes()).hexdigest()
              for path in Path(directory).glob('*.so*') if path.is_file()}
    require(bindings == actual, 'Container payload differs from exact tested ZIP')
    return {'bundle_sha256': hashlib.sha256(Path(bundle).read_bytes()).hexdigest(), 'library_sha256': bindings}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bundle', required=True)
    parser.add_argument('--directory', required=True)
    parser.add_argument('--arch', required=True, choices=['amd64', 'arm64'])
    parser.add_argument('--output', required=True)
    args = parser.parse_args()
    Path(args.output).write_text(json.dumps(verify(args.bundle, args.directory, args.arch), indent=2) + '\n')
