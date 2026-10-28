#!/usr/bin/env python3
"""Synthetic sensitivity checks; no Babel, Git writes, or runtime evidence reads."""
import hashlib
import json
from pathlib import Path
import unittest
from unittest.mock import patch

import verify_failure_recovery as verifier

SHA = 'c' * 40
SESSION = 'independently-observed-success-session'
MODEL = 'deepseek-v4.1-flash'
SESSION_KEY = 's_' + hashlib.sha256(('zcode:' + SESSION).encode()).hexdigest()[:14]
EXPECTED, ABSENT, PLAN_HASHES = verifier.load_expectations()


def responses(*, source=None, contents=None, extra_change=False, extra_file=False,
              rejected=False, modified=False, missing_note=False):
    source = source or {}
    contents = contents or {}
    paths = sorted(EXPECTED)
    oids = {path: str(i + 1) * 40 for i, path in enumerate(paths)}
    changes = [(path, 'M' if modified and i == 0 else 'A') for i, path in enumerate(paths)]
    if extra_change:
        changes.append((verifier.FIXTURE_ROOT + 'extra.txt', 'A'))
    raw = (SHA + '\n' + 'b' * 40 + '\0').encode()
    for path, status in changes:
        raw += (f'\n:000000 100644 {"0" * 40} {oids.get(path, "8" * 40)} {status}\0{path}\0').encode()
    tree = {path: ('100644', 'blob', oid) for path, oid in oids.items()}
    tree[verifier.FIXTURE_ROOT + 'lineage.txt'] = ('100644', 'blob', '5' * 40)
    if extra_file:
        tree[verifier.FIXTURE_ROOT + 'extra.txt'] = ('100644', 'blob', '8' * 40)
    if rejected:
        tree[next(iter(ABSENT))] = ('100644', 'blob', '7' * 40)
    tree_bytes = b''.join(f'{mode} {kind} {oid}\t{path}\0'.encode()
                          for path, (mode, kind, oid) in sorted(tree.items()))
    note_lines = []
    for path in paths:
        kind = source.get(path, EXPECTED[path]['kind'])
        if kind == 'known_human':
            note_lines.append(path + '\n  h_manual 1')
        elif kind == 'ai':
            if source.get('overlap_human'):
                note_lines.append(path + '\n  h_manual 1')
            note_lines.append(path + '\n  ' + SESSION_KEY + '::t_success 1')
    metadata = {'schema_version': 'authorship/3.0.0', 'humans': {'h_manual': {}},
                'sessions': {SESSION_KEY: {'agent_id': {'tool': source.get('tool', 'zcode'),
                   'id': source.get('session', SESSION), 'model': 'unrelated-final-model'},
                   'trace_models': {} if source.get('missing_trace') else
                       {'t_success': source.get('model', MODEL)}}}}
    note = '\n'.join(note_lines) + '\n---\n' + json.dumps(metadata)
    note_oid = '9' * 40
    blobs = {oids[path]: contents.get(path, EXPECTED[path]['content']).encode() for path in paths}
    if not missing_note:
        blobs[note_oid] = note.encode()
    batch = b''.join(f'{oid} blob {len(data)}\n'.encode() + data + b'\n'
                     for oid, data in sorted(blobs.items()))
    index = b'' if missing_note else f'100644 blob {note_oid}\t{SHA}\0'.encode()
    return [raw, tree_bytes, index, batch]


def run_case(**kwargs):
    with patch.object(verifier, 'git_read', side_effect=responses(**kwargs)) as reads:
        result = verifier.verify(Path('/unused'), 'HEAD', EXPECTED, ABSENT, SESSION, MODEL)
    assert reads.call_count == 4
    return result


class FailureRecoveryTests(unittest.TestCase):
    def test_frozen_plans_and_exact_three_files_pass_with_four_git_reads(self):
        self.assertEqual(len(EXPECTED), 3)
        self.assertEqual(sorted(row['kind'] for row in EXPECTED.values()), ['ai', 'unknown', 'unknown'])
        self.assertEqual(len(ABSENT), 1)
        self.assertEqual(len(PLAN_HASHES), 2)
        self.assertTrue(run_case()['passed'])

    def test_unknown_never_accepts_known_human_or_ai(self):
        for path, expected in EXPECTED.items():
            if expected['kind'] == 'unknown':
                for kind in ['known_human', 'ai']:
                    self.assertFalse(run_case(source={path: kind})['passed'])

    def test_wrong_ai_identity_and_missing_per_trace_model_fail(self):
        for source in [{'tool': 'claude'}, {'session': 'other'}, {'model': 'other'},
                       {'missing_trace': True}]:
            self.assertFalse(run_case(source=source)['passed'])

    def test_overlapping_known_human_cannot_hide_behind_last_ai_entry(self):
        self.assertFalse(run_case(source={'overlap_human': True})['passed'])

    def test_extra_changed_or_existing_fixture_rejection_and_non_added_fail(self):
        for option in ['extra_change', 'extra_file', 'rejected', 'modified']:
            self.assertFalse(run_case(**{option: True})['passed'])

    def test_exact_line_newline_and_target_note_required(self):
        path = next(iter(EXPECTED))
        for content in ['WRONG\n', EXPECTED[path]['content'] + '\n', EXPECTED[path]['content'].rstrip('\n')]:
            self.assertFalse(run_case(contents={path: content})['passed'])
        self.assertFalse(run_case(missing_note=True)['passed'])


if __name__ == '__main__':
    unittest.main()
