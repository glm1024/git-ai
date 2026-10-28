#!/usr/bin/env python3
"""Verify frozen cancellation/denial recovery with four read-only batched Git calls."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys

from verify_oracle import (EvidenceError, actual_author, compare_author, git_read,
                           parse_batch_objects, parse_note)

FIXTURE_ROOT = 'tests/fixtures/zcode_deep_20261028_v1/'
PRIOR_FILES = {FIXTURE_ROOT + 'lineage.txt', FIXTURE_ROOT + 'catalog.jsonl'}
PLAN_HASHES = {
    'after-fix-plan.json': 'a80c5071a666ecb4db2aeac0e485fc4f5037ec49c7fd5c30004c24eefce04ce4',
    'cancellation-plan.json': '7aa6f8ddd05a73d8af2b3c8a27aad24c58ab4602aebb919a897be7e868c29086',
}
OID = re.compile(r'[0-9a-f]{40}(?:[0-9a-f]{24})?')


def load_expectations():
    plans = {}
    for name, digest in PLAN_HASHES.items():
        raw = Path(__file__).with_name(name).read_bytes()
        if hashlib.sha256(raw).hexdigest() != digest:
            raise EvidenceError('Frozen plan hash changed: ' + name)
        plans[name] = json.loads(raw)
    fixed = plans['after-fix-plan.json']
    cancelled = plans['cancellation-plan.json']
    if fixed.get('frozen_before_execution') is not True:
        raise EvidenceError('After-fix plan must have been frozen before execution')
    expected = {cancelled['file']: {'kind': 'unknown', 'content': cancelled['content']}}
    absent = set()
    for scenario in fixed['scenarios']:
        if scenario.get('expected_exists') is False:
            absent.add(scenario['file'])
            continue
        if (scenario['expected_ai'], scenario['expected_unknown']) not in [(0, 1), (1, 0)]:
            raise EvidenceError('Unsupported frozen source counts')
        expected[scenario['file']] = {'content': scenario['content'],
                                      'kind': 'ai' if scenario['expected_ai'] else 'unknown'}
    if len(expected) != 3 or len(absent) != 1 or expected.keys() & absent:
        raise EvidenceError('Frozen plans do not declare exactly three files and one absence')
    for path, row in expected.items():
        if not path.startswith(FIXTURE_ROOT) or '/' in path[len(FIXTURE_ROOT):]:
            raise EvidenceError('Frozen fixture path is outside the declared directory')
        if not row['content'].endswith('\n') or len(row['content'].splitlines()) != 1:
            raise EvidenceError('Frozen fixture must contain exactly one newline-terminated line')
    return expected, absent, dict(PLAN_HASHES)


def parse_tree(raw):
    entries = {}
    for entry in raw.split(b'\0'):
        if not entry:
            continue
        info, path = entry.decode('utf-8').split('\t', 1)
        mode, kind, oid = info.split()
        if path in entries or not OID.fullmatch(oid):
            raise EvidenceError('Invalid or duplicate tree entry: ' + path)
        entries[path] = (mode, kind, oid)
    return entries


def parse_changes(raw):
    tokens = raw.decode('utf-8').split('\0')
    header = tokens.pop(0).splitlines()
    if len(header) != 2 or not all(OID.fullmatch(value) for value in header):
        raise EvidenceError('Target must resolve to one ordinary, non-merge commit with fixture changes')
    changes = {}
    while tokens and tokens[0]:
        info = tokens.pop(0).lstrip('\n')
        if not tokens or not info.startswith(':'):
            raise EvidenceError('Malformed raw change record')
        old_mode, new_mode, old_oid, new_oid, status = info[1:].split()
        path = tokens.pop(0)
        if path in changes or not OID.fullmatch(old_oid) or not OID.fullmatch(new_oid):
            raise EvidenceError('Invalid or duplicate raw change: ' + path)
        changes[path] = {'status': status, 'mode': new_mode, 'oid': new_oid,
                         'old_mode': old_mode, 'old_oid': old_oid}
    if any(tokens):
        raise EvidenceError('Unexpected trailing raw change data')
    return header[0], changes


def verify(repo, commit, expected, absent, session, model):
    if not commit or commit.startswith('-') or '\n' in commit or '\0' in commit:
        raise EvidenceError('Invalid commit/ref argument')
    # show emits only raw metadata, never patches; path restriction is the full
    # fixture directory, so extra files cannot escape the change-set check.
    sha, changes = parse_changes(git_read(
        repo, 'show', '--format=%H%n%P', '--raw', '-z', '--no-renames', '--root',
        '--no-abbrev', f'{commit}^{{commit}}', '--', FIXTURE_ROOT))
    tree = parse_tree(git_read(repo, 'ls-tree', '-r', '-z', sha, '--', FIXTURE_ROOT))
    index = parse_tree(git_read(repo, 'ls-tree', '-r', '-z', 'refs/notes/ai'))
    note_index = {path.replace('/', ''): oid for path, (_, kind, oid) in index.items() if kind == 'blob'}
    errors = []
    if set(changes) != set(expected):
        errors.append('Changed fixture files differ from the frozen three additions')
    if set(tree) - set(expected) - PRIOR_FILES:
        errors.append('Unexpected fixture files exist: ' + ', '.join(sorted(set(tree) - set(expected) - PRIOR_FILES)))
    if absent & set(tree):
        errors.append('Denied fixture must not exist: ' + ', '.join(sorted(absent & set(tree))))
    blobs = {}
    for path in expected:
        entry = tree.get(path)
        change = changes.get(path)
        if entry is None:
            errors.append('Missing expected fixture: ' + path)
            continue
        if entry[:2] != ('100644', 'blob'):
            errors.append('Fixture must be an ordinary text blob: ' + path)
            continue
        blobs[path] = entry[2]
        if (not change or change['status'] != 'A' or change['mode'] != '100644'
                or change['old_mode'] != '000000' or set(change['old_oid']) != {'0'}
                or change['oid'] != entry[2]):
            errors.append('Fixture must be newly added in target commit: ' + path)
    note_oid = note_index.get(sha)
    if note_oid is None:
        errors.append('Target commit authorship note is missing')
    oids = sorted(set(blobs.values()) | ({note_oid} if note_oid else set()))
    # Always exactly the fourth Git invocation, including incomplete evidence.
    objects = parse_batch_objects(git_read(repo, 'cat-file', '--batch',
        input_bytes=('\n'.join(oids) + ('\n' if oids else '')).encode()))
    note = parse_note(objects[note_oid]) if note_oid else None
    bindings = {'success': {'session': session, 'model': model, 'traces': []}}
    rows = []
    for path, expectation in sorted(expected.items()):
        content = objects.get(blobs.get(path))
        row_errors = []
        if content != expectation['content']:
            row_errors.append('Exact text/line count/newline differs from frozen plan')
        attestations = [author for author, ranges in (note['files'].get(path, []) if note else [])
                        if any(low <= 1 <= high for low, high in ranges)]
        if len(attestations) > 1:
            row_errors.append('Overlapping line attestations; a later AI entry cannot conceal KnownHuman')
        source = actual_author(note, path, 1)
        desired = {'kind': expectation['kind'], 'generation': 'success', 'tool': 'zcode'}
        row_errors.extend(compare_author(desired, source, bindings, require_trace_models=True))
        rows.append({'file': path, 'line': 1, 'expected_text': expectation['content'],
                     'actual_text': content, 'expected': desired, 'actual': source,
                     'all_line_attestations': attestations,
                     'passed': not row_errors, 'errors': row_errors})
    return {'schema': 'zcode-independent-failure-recovery/v1', 'repo': str(repo), 'commit': sha,
            'bindings': bindings, 'git_read_count': 4,
            'expected_absent': sorted(absent), 'actual_fixture_files': sorted(tree),
            'actual_changed_files': {path: item['status'] for path, item in sorted(changes.items())},
            'expected_counts': {'ai': 1, 'unknown': 2, 'known_human': 0},
            'actual_counts': {kind: sum(row['actual']['kind'] == kind for row in rows)
                              for kind in ['ai', 'unknown', 'known_human', 'unresolved']},
            'rows': rows, 'errors': errors,
            'passed': not errors and all(row['passed'] for row in rows)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', type=Path, required=True)
    parser.add_argument('--commit', required=True)
    parser.add_argument('--success-session', required=True,
                        help='Actual successful ZCode session captured independently of the note')
    parser.add_argument('--model', required=True)
    parser.add_argument('--output', type=Path,
        default=Path(__file__).with_name('evidence') / 'failure-recovery-oracle.json')
    args = parser.parse_args()
    repo, output = args.repo.resolve(), args.output.resolve()
    if output == repo or repo in output.parents:
        parser.error('--output must be outside the tested repository')
    protected = {Path(__file__).with_name(name).resolve() for name in
                 [*PLAN_HASHES, 'oracle.json', 'catalog-oracle.json', Path(__file__).name]}
    if output in protected:
        parser.error('--output must not overwrite a frozen plan/oracle or the verifier')
    if not args.success_session.strip() or not args.model.strip():
        parser.error('Independent success session and model bindings must be nonempty')
    try:
        expected, absent, hashes = load_expectations()
        result = verify(repo, args.commit, expected, absent, args.success_session, args.model)
        result['frozen_plan_sha256'] = hashes
    except (EvidenceError, ValueError, KeyError, OSError, subprocess.TimeoutExpired) as error:
        result = {'schema': 'zcode-independent-failure-recovery/v1', 'repo': str(repo),
                  'passed': False, 'evidence_error': str(error)}
    text = json.dumps(result, ensure_ascii=False, indent=2) + '\n'
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(text)
    sys.stdout.write(text)
    return 0 if result['passed'] else 1


if __name__ == '__main__':
    sys.exit(main())
