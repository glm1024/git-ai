#!/usr/bin/env python3
"""Synthetic sensitivity checks for the frozen batch verifier, no real Git writes."""
import copy
import hashlib
import json
from pathlib import Path
import unittest
from unittest.mock import patch

import verify_catalog as verifier

COMMIT = 'c' * 40
SESSION = 'observed-batch-session'
SESSION_KEY = 's_' + hashlib.sha256(('zcode:' + SESSION).encode()).hexdigest()[:14]
ORACLE = json.loads(Path(__file__).with_name('catalog-oracle.json').read_text())


def note_text(tool='zcode'):
    metadata = {'schema_version': 'authorship/3.0.0', 'prompts': {},
                'sessions': {SESSION_KEY: {'agent_id': {'tool': tool, 'id': SESSION, 'model': 'final-other-model'},
                                           'trace_models': {'t_batch': 'deepseek-v4.1-flash'}}}}
    return ORACLE['file'] + '\n  ' + SESSION_KEY + '::t_batch 1-64\n---\n' + json.dumps(metadata)


def run_content(content, tool='zcode', change='A'):
    replies = [COMMIT.encode() + b'\n', content.encode(),
               (change + '\t' + ORACLE['file'] + '\n').encode(), note_text(tool).encode()]
    with patch.object(verifier, 'git_read', side_effect=replies) as reads:
        result = verifier.verify_catalog(Path('/unused'), 'HEAD', ORACLE, SESSION, 'deepseek-v4.1-flash', [])
    assert reads.call_count == 4
    return result


class CatalogTests(unittest.TestCase):
    def test_frozen_64_records_and_whitespace_independence(self):
        self.assertEqual(len(ORACLE['records']), 64)
        text = '\n'.join(json.dumps({'enabled': row['enabled'], 'code': row['code'], 'id': row['id']}, separators=(', ', ': '))
                         for row in ORACLE['records']) + '\n'
        self.assertTrue(run_content(text)['passed'])
        self.assertEqual(hashlib.sha256(verifier.canonical_records(ORACLE['records']).encode()).hexdigest(), ORACLE['canonical_records_sha256'])

    def test_numeric_boolean_extra_field_duplicate_id_and_wrong_order_fail(self):
        examples = []
        records = copy.deepcopy(ORACLE['records'])
        records[0]['enabled'] = 0
        examples.append(records)
        records = copy.deepcopy(ORACLE['records'])
        records[0]['extra'] = True
        examples.append(records)
        records = copy.deepcopy(ORACLE['records'])
        records[1]['id'] = 1
        examples.append(records)
        records = copy.deepcopy(ORACLE['records'])
        records[62], records[63] = records[63], records[62]
        examples.append(records)
        for records in examples:
            self.assertFalse(run_content(verifier.canonical_records(records))['passed'])

    def test_duplicate_json_keys_and_extra_blank_line_fail(self):
        lines = verifier.canonical_records(ORACLE['records']).splitlines()
        lines[0] = '{"id":1,"id":1,"code":"ZDB-001","enabled":false}'
        self.assertFalse(run_content('\n'.join(lines) + '\n')['passed'])
        self.assertFalse(run_content(verifier.canonical_records(ORACLE['records']) + '\n')['passed'])

    def test_wrong_tool_or_non_added_file_fails_even_when_objects_match(self):
        content = verifier.canonical_records(ORACLE['records'])
        self.assertFalse(run_content(content, tool='claude')['passed'])
        self.assertFalse(run_content(content, change='M')['passed'])


if __name__ == '__main__':
    unittest.main()
