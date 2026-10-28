#!/usr/bin/env python3
"""Print a frozen stage target; never edits the test repository."""
import argparse
import json
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('phase', choices=['baseline', 'round1', 'human', 'commit1', 'round2', 'commit2', 'amend'])
parser.add_argument('--oracle', type=Path, default=Path(__file__).with_name('oracle.json'))
parser.add_argument('--json', action='store_true', help='Print the operation and expected source metadata too.')
args = parser.parse_args()
oracle = json.loads(args.oracle.read_text())
phase = oracle['phases'][args.phase]
if args.json:
    print(json.dumps({'file': oracle['file'], 'phase': args.phase, **phase}, ensure_ascii=False, indent=2))
else:
    print('\n'.join(phase['lines']))
