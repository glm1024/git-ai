#!/usr/bin/env python3
"""Read-only independent 64-row catalog oracle; shared note parser, no Babel writes."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys

from verify_oracle import EvidenceError, actual_author, compare_author, git_read, parse_note


def unique_object(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise EvidenceError('Duplicate JSON key: ' + key)
        value[key] = item
    return value


def record_errors(expected, actual):
    if not isinstance(actual, dict):
        return ['JSONL row is not an object']
    if actual.keys() != expected.keys():
        return ['Object fields differ from frozen id/code/enabled schema']
    errors = []
    for key, expected_value in expected.items():
        if type(actual[key]) is not type(expected_value) or actual[key] != expected_value:
            errors.append(f'Field {key}: wrong type or value')
    return errors


def canonical_records(records):
    return '\n'.join(json.dumps(row, sort_keys=True, separators=(',', ':')) for row in records) + '\n'


def verify_catalog(repo, commit, oracle, session, model, traces):
    sha = git_read(repo, 'rev-parse', '--verify', f'{commit}^{{commit}}').decode().strip()
    content = git_read(repo, 'show', f"{sha}:{oracle['file']}").decode('utf-8')
    change = git_read(repo, 'diff-tree', '--root', '--no-commit-id', '--name-status', '-r',
                      sha, '--', oracle['file']).decode().strip()
    note = parse_note(git_read(repo, 'notes', '--ref=refs/notes/ai', 'show', sha).decode('utf-8'))
    lines = content.splitlines()
    errors = []
    if change != 'A\t' + oracle['file']:
        errors.append('Fixture must be newly added in the tested commit; no ancestor attribution is inferred')
    if len(lines) != oracle['row_count']:
        errors.append(f"Expected {oracle['row_count']} physical object lines, got {len(lines)}")
    rows = []
    parsed_rows = []
    ids = []
    expected_source = {'kind': oracle['expected_kind'], 'generation': 'batch', 'tool': oracle['expected_tool']}
    bindings = {'batch': {'session': session, 'model': model, 'traces': traces}}
    for number, expected in enumerate(oracle['records'], 1):
        row_errors = []
        actual = None
        if number > len(lines):
            row_errors.append('Missing JSONL row')
        else:
            try:
                actual = json.loads(lines[number - 1], object_pairs_hook=unique_object)
                row_errors.extend(record_errors(expected, actual))
                parsed_rows.append(actual)
                if isinstance(actual, dict) and type(actual.get('id')) is int:
                    ids.append(actual['id'])
            except (ValueError, EvidenceError) as error:
                row_errors.append(str(error))
        attribution = actual_author(note, oracle['file'], number)
        row_errors.extend(compare_author(expected_source, attribution, bindings, oracle.get('require_trace_models', True)))
        rows.append({'line': number, 'expected': expected, 'actual': actual,
                     'source': attribution, 'passed': not row_errors, 'errors': row_errors})
    if len(ids) != len(set(ids)):
        errors.append('Duplicate row IDs')
    digest = hashlib.sha256(canonical_records(parsed_rows).encode()).hexdigest()
    if digest != oracle['canonical_records_sha256']:
        errors.append('Canonical object sequence differs from the frozen oracle')
    return {'schema': 'zcode-independent-batch-verification/v1', 'scenario': oracle['scenario_id'],
            'repo': str(repo), 'commit': sha, 'file': oracle['file'], 'bindings': bindings,
            'expected_row_count': oracle['row_count'], 'actual_row_count': len(lines),
            'canonical_records_sha256': digest, 'expected_records_sha256': oracle['canonical_records_sha256'],
            'rows': rows, 'errors': errors, 'passed': not errors and all(row['passed'] for row in rows)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', type=Path, required=True)
    parser.add_argument('--commit', required=True)
    parser.add_argument('--session', required=True, help='Real batch session from ZCode UI/source, never inferred from notes')
    parser.add_argument('--model')
    parser.add_argument('--trace', action='append', default=[])
    parser.add_argument('--oracle', type=Path, default=Path(__file__).with_name('catalog-oracle.json'))
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    repo = args.repo.resolve()
    if args.output and (args.output.resolve() == repo or repo in args.output.resolve().parents):
        parser.error('--output must be outside the tested repository')
    if args.output and args.output.resolve() == args.oracle.resolve():
        parser.error('--output must not overwrite the frozen oracle')
    oracle_bytes = args.oracle.read_bytes()
    oracle = json.loads(oracle_bytes)
    try:
        result = verify_catalog(repo, args.commit, oracle, args.session, args.model or oracle['model_default'], args.trace)
    except (EvidenceError, ValueError, KeyError, OSError, subprocess.TimeoutExpired) as error:
        result = {'schema': 'zcode-independent-batch-verification/v1', 'repo': str(repo),
                  'passed': False, 'evidence_error': str(error)}
    result['oracle_sha256'] = hashlib.sha256(oracle_bytes).hexdigest()
    output = json.dumps(result, ensure_ascii=False, indent=2) + '\n'
    if args.output:
        args.output.write_text(output)
    sys.stdout.write(output)
    return 0 if result['passed'] else 1


if __name__ == '__main__':
    sys.exit(main())
