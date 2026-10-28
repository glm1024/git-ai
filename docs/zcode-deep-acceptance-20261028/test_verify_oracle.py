#!/usr/bin/env python3
"""Self-tests of the verifier, not claims about Git AI or the real UI run."""
import copy
import hashlib
import json
from pathlib import Path
import unittest
from unittest.mock import patch

import verify_oracle as verifier

SESSION = 'external-round1'
SESSION_KEY = 's_' + hashlib.sha256(('zcode:' + SESSION).encode()).hexdigest()[:14]
AUTHOR = SESSION_KEY + '::t_fixture'
TEXT = 'ZDA-H01 | manually seeded\nZDA-A01 | generated alpha\n'
COMMIT = 'a' * 40
NOTE_OID = 'b' * 40
BINDINGS = {'round1': {'session': SESSION, 'model': 'model-r1', 'traces': []}}


def note_text(ranges='2'):
    metadata = {'schema_version': 'authorship/3.0.0', 'prompts': {}, 'humans': {},
                'sessions': {SESSION_KEY: {'agent_id': {'tool': 'zcode', 'id': SESSION, 'model': 'wrong-final-model'},
                                           'trace_models': {'t_fixture': 'model-r1'}}}}
    return 'fixture.txt\n  ' + AUTHOR + ' ' + ranges + '\n---\n' + json.dumps(metadata)


def sample_oracle():
    return {'scenario_id': 'verifier-self-test', 'file': 'fixture.txt', 'require_trace_models': True,
            'phases': {'commit1': {'lines': TEXT.splitlines(),
                                   'text_sha256': hashlib.sha256(TEXT.encode()).hexdigest(),
                                   'expected_counts': {'ai': 1, 'unknown': 1, 'known_human': 0},
                                   'expectations': [{'id': 'ZDA-H01', 'kind': 'unknown'},
                                                    {'id': 'ZDA-A01', 'kind': 'ai', 'generation': 'round1', 'tool': 'zcode'}]}}}


def git_responses(note):
    encoded = note.encode()
    blame = ''.join(f'{COMMIT} {number} {number} 1\nauthor Fixture\nfilename fixture.txt\n\t{text}\n'
                    for number, text in enumerate(TEXT.splitlines(), 1))
    return [COMMIT.encode() + b'\n', TEXT.encode(), blame.encode(),
            f'100644 blob {NOTE_OID}\t{COMMIT}\n'.encode(),
            f'{NOTE_OID} blob {len(encoded)}\n'.encode() + encoded + b'\n']


class OracleTests(unittest.TestCase):
    def test_per_trace_model_wins_over_session_final_model(self):
        parsed = verifier.parse_note(note_text())
        actual = verifier.actual_author(parsed, 'fixture.txt', 2)
        self.assertEqual(actual['model'], 'model-r1')
        self.assertEqual(actual['model_evidence'], 'trace')
        self.assertEqual(verifier.compare_author(sample_oracle()['phases']['commit1']['expectations'][1], actual, BINDINGS), [])
        self.assertEqual(verifier.actual_author(parsed, 'fixture.txt', 1)['kind'], 'unknown')

    def test_manual_unknown_is_not_known_human_or_ai(self):
        expected = {'kind': 'unknown'}
        self.assertTrue(verifier.compare_author(expected, {'kind': 'known_human'}, BINDINGS))
        self.assertTrue(verifier.compare_author(expected, {'kind': 'ai'}, BINDINGS))

    def test_missing_external_binding_or_wrong_model_is_not_accepted(self):
        actual = verifier.actual_author(verifier.parse_note(note_text()), 'fixture.txt', 2)
        expected = {'kind': 'ai', 'generation': 'round1', 'tool': 'zcode'}
        missing = copy.deepcopy(BINDINGS)
        missing['round1']['session'] = None
        self.assertTrue(verifier.compare_author(expected, actual, missing))
        actual['model'] = 'other-model'
        self.assertTrue(verifier.compare_author(expected, actual, BINDINGS))

    def test_blame_is_only_an_actual_coordinate_lookup(self):
        parsed = verifier.parse_blame(f'{COMMIT} 7 12 1\nauthor Fixture\nfilename fixture.txt\n\tZDA-A06 | unchanged\n')
        self.assertEqual(parsed[12]['origin_line'], 7)
        self.assertEqual(parsed[12]['text'], 'ZDA-A06 | unchanged')

    def test_cat_file_batch_requires_complete_exact_frames(self):
        self.assertEqual(verifier.parse_batch_objects(b'abc blob 3\nxyz\n'), {'abc': 'xyz'})
        with self.assertRaises(verifier.EvidenceError):
            verifier.parse_batch_objects(b'abc blob 4\nxyz\n')

    def test_full_pipeline_uses_five_reads_and_rejects_false_ai_claim(self):
        for ranges, expected_pass in [('2', True), ('1-2', False)]:
            with patch.object(verifier, 'git_read', side_effect=git_responses(note_text(ranges))) as reads:
                result = verifier.verify(Path('/unused'), 'HEAD', 'commit1', sample_oracle(), BINDINGS)
            self.assertEqual(reads.call_count, 5)
            self.assertEqual(result['passed'], expected_pass)
            if not expected_pass:
                self.assertFalse(result['rows'][0]['passed'])

    def test_frozen_scenario_counts_and_unique_ids(self):
        oracle = json.loads(Path(__file__).with_name('oracle.json').read_text())
        for name, phase in oracle['phases'].items():
            self.assertLessEqual(len(phase['lines']), 20, name)
            ids = [text.split(' | ', 1)[0] for text in phase['lines']]
            self.assertEqual(len(set(ids)), len(ids), name)
            self.assertTrue(all(text.startswith('ZDA-') for text in ids))
            content = '\n'.join(phase['lines']) + '\n'
            self.assertEqual(hashlib.sha256(content.encode()).hexdigest(), phase['text_sha256'])
        self.assertEqual(oracle['phases']['commit1']['expected_counts']['ai'], 2)
        self.assertEqual(oracle['phases']['commit2']['expected_counts']['ai'], 4)
        self.assertEqual(oracle['phases']['amend']['expected_counts']['ai'], 6)


if __name__ == '__main__':
    unittest.main()
