#!/usr/bin/env python3
"""Read-only independent line oracle for the declared ZCode acceptance fixture.

Expected text/source comes ONLY from oracle.json. Ordinary Git blame is used
only to locate each actual line's origin note, never to generate expectations.
Five fixed Git subprocesses resolve the commit, show text, locate source lines,
list note object IDs, and batch-read all necessary notes. No git-ai process,
repository write, per-line Git spawn, historical repair, or network call occurs.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import unicodedata


class EvidenceError(Exception):
    pass


def git_read(repo, *arguments, input_bytes=None):
    environment = os.environ.copy()
    environment['GIT_OPTIONAL_LOCKS'] = '0'
    result = subprocess.run(
        ['git', '--no-optional-locks', '-C', str(repo), *arguments],
        input=input_bytes, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        env=environment, timeout=60, check=False,
    )
    if result.returncode:
        raise EvidenceError(f'git {arguments[0]} failed ({result.returncode}): '
                            + result.stderr.decode('utf-8', errors='replace').strip())
    return result.stdout


def parse_note(text):
    match = re.search(r'(?m)^---\r?$', text)
    if not match:
        raise EvidenceError('Authorship note has no metadata divider')
    metadata = json.loads(text[match.end():].strip())
    if metadata.get('schema_version') != 'authorship/3.0.0':
        raise EvidenceError('Unsupported authorship schema: ' + str(metadata.get('schema_version')))
    files = {}
    current = None
    for line in text[:match.start()].splitlines():
        if not line.strip():
            continue
        if line.startswith('  '):
            if current is None:
                raise EvidenceError('Attestation has no file header')
            author, ranges_text = line[2:].split(' ', 1)
            ranges = []
            for value in ranges_text.split(','):
                values = value.strip().split('-')
                if len(values) > 2 or not all(part.isdecimal() for part in values):
                    raise EvidenceError('Invalid attestation range: ' + value)
                low, high = int(values[0]), int(values[-1])
                if low < 1 or high < low:
                    raise EvidenceError('Invalid attestation bounds')
                ranges.append((low, high))
            files[current].append((author, ranges))
        else:
            current = unicodedata.normalize('NFC', line.strip('"'))
            files.setdefault(current, [])
    return {'files': files, 'metadata': metadata}


def parse_blame(text):
    actual = {}
    header = None
    for line in text.splitlines():
        matched = re.fullmatch(r'\^?([0-9a-f]{40,64}) (\d+) (\d+)(?: \d+)?', line)
        if matched:
            header = (matched[1], int(matched[2]), int(matched[3]))
        elif line.startswith('\t'):
            if header is None:
                raise EvidenceError('Blame text has no origin header')
            commit, origin_line, final_line = header
            if final_line in actual:
                raise EvidenceError('Duplicate final blame coordinate')
            actual[final_line] = {'commit': commit, 'origin_line': origin_line, 'text': line[1:]}
            header = None
    return actual


def parse_batch_objects(data):
    objects = {}
    offset = 0
    while offset < len(data):
        end = data.find(b'\n', offset)
        if end < 0:
            raise EvidenceError('Truncated cat-file header')
        header = data[offset:end].decode('ascii').split()
        if len(header) != 3 or header[1] != 'blob':
            raise EvidenceError('Missing/non-blob note object: ' + ' '.join(header))
        size = int(header[2])
        start = end + 1
        finish = start + size
        if finish >= len(data) or data[finish:finish + 1] != b'\n':
            raise EvidenceError('Truncated cat-file body')
        objects[header[0]] = data[start:finish].decode('utf-8')
        offset = finish + 1
    return objects


def actual_author(note, file_path, line):
    if note is None:
        return {'kind': 'unknown', 'evidence': 'no-origin-note'}
    entries = note['files'].get(unicodedata.normalize('NFC', file_path), [])
    for author, ranges in reversed(entries):
        if not any(low <= line <= high for low, high in ranges):
            continue
        metadata = note['metadata']
        if author.startswith('h_'):
            return {'kind': 'known_human', 'author_id': author,
                    'human_record_present': author in metadata.get('humans', {})}
        session_key, separator, trace = author.partition('::')
        record = metadata.get('sessions', {}).get(session_key)
        if record is None:
            record = metadata.get('prompts', {}).get(author)
        if not isinstance(record, dict) or not isinstance(record.get('agent_id'), dict):
            return {'kind': 'unresolved', 'author_id': author, 'error': 'Missing agent metadata'}
        agent = record['agent_id']
        trace_model = record.get('trace_models', {}).get(trace) if separator else None
        has_trace_model = isinstance(trace_model, str) and bool(trace_model.strip())
        return {'kind': 'ai', 'author_id': author, 'session_key': session_key,
                'trace': trace if separator else None, 'tool': agent.get('tool'),
                'session': agent.get('id'),
                'model': trace_model if has_trace_model else agent.get('model'),
                'model_evidence': 'trace' if has_trace_model else 'session-fallback'}
    return {'kind': 'unknown', 'evidence': 'no-line-attestation'}


def compare_author(expected, actual, bindings, require_trace_models=True):
    errors = []
    if expected['kind'] != actual['kind']:
        return [f"source kind: expected {expected['kind']}, got {actual['kind']}"]
    if expected['kind'] != 'ai':
        return errors
    generation = expected['generation']
    binding = bindings[generation]
    if actual.get('tool') != expected['tool']:
        errors.append(f"tool: expected {expected['tool']}, got {actual.get('tool')}")
    if not binding.get('session'):
        errors.append(f'{generation} session binding missing; obtain it from the UI/hook/source, not the note')
    elif actual.get('session') != binding['session']:
        errors.append(f"session: expected {binding['session']}, got {actual.get('session')}")
    if actual.get('model') != binding['model']:
        errors.append(f"model: expected {binding['model']}, got {actual.get('model')}")
    if require_trace_models and actual.get('model_evidence') != 'trace':
        errors.append('Per-trace model evidence missing')
    if binding.get('traces') and actual.get('trace') not in binding['traces']:
        errors.append(f"trace does not belong to observed {generation} traces")
    if actual.get('session_key', '').startswith('s_'):
        digest = hashlib.sha256(f"{actual.get('tool')}:{actual.get('session')}".encode()).hexdigest()
        if actual['session_key'] != 's_' + digest[:14]:
            errors.append('Session hash disagrees with its declared external session/tool')
    return errors


def verify(repo, commit, phase_name, oracle, bindings):
    phase = oracle['phases'][phase_name]
    file_path = oracle['file']
    commit_sha = git_read(repo, 'rev-parse', '--verify', f'{commit}^{{commit}}').decode().strip()
    content = git_read(repo, 'show', f'{commit_sha}:{file_path}').decode('utf-8')
    expected_content = '\n'.join(phase['lines']) + '\n'
    actual_lines = content.splitlines()
    origins = parse_blame(git_read(repo, 'blame', '--line-porcelain', commit_sha, '--', file_path).decode('utf-8'))
    note_index = {}
    for line in git_read(repo, 'ls-tree', '-r', 'refs/notes/ai').decode().splitlines():
        info, path = line.split('\t', 1)
        mode, kind, oid = info.split()
        if kind == 'blob':
            note_index[path.replace('/', '')] = oid
    commits = {value['commit'] for value in origins.values()} | {commit_sha}
    note_oids = sorted({note_index[sha] for sha in commits if sha in note_index})
    objects = parse_batch_objects(git_read(repo, 'cat-file', '--batch', input_bytes=('\n'.join(note_oids) + '\n').encode())) if note_oids else {}
    notes = {sha: parse_note(objects[note_index[sha]]) for sha in commits if sha in note_index}
    errors = []
    if commit_sha not in notes:
        errors.append('Target commit note is missing; no attribution receipt is verified')
    if content != expected_content:
        errors.append('Committed fixture text/order/newline differs from the frozen target')
    rows = []
    for number, (expected_text, expected) in enumerate(zip(phase['lines'], phase['expectations']), 1):
        origin = origins.get(number)
        row_errors = []
        actual_text = actual_lines[number - 1] if number <= len(actual_lines) else None
        if actual_text != expected_text:
            row_errors.append('Line text differs from frozen target')
        if origin is None or origin['text'] != actual_text:
            row_errors.append('Missing/inconsistent ordinary Git origin coordinate')
        actual = actual_author(notes.get(origin['commit']), file_path, origin['origin_line']) if origin else {'kind': 'unresolved'}
        row_errors.extend(compare_author(expected, actual, bindings, oracle.get('require_trace_models', True)))
        rows.append({'line': number, 'id': expected['id'], 'text': actual_text,
                     'expected': expected, 'actual': actual, 'origin': origin,
                     'passed': not row_errors, 'errors': row_errors})
    counts = {kind: sum(row['actual']['kind'] == kind for row in rows)
              for kind in ['ai', 'unknown', 'known_human', 'unresolved']}
    return {'schema': 'zcode-independent-line-verification/v1', 'scenario': oracle['scenario_id'],
            'repo': str(repo), 'commit': commit_sha, 'phase': phase_name, 'file': file_path,
            'verification_scope': 'exact text, line kind, tool, external session, per-trace model; optional trace bindings',
            'expected_generation_note': 'If rounds share session/model, separate generation trace identity is checked only when --trace-roundN is supplied.',
            'bindings': bindings, 'expected_counts': phase['expected_counts'], 'actual_counts': counts,
            'expected_text_sha256': phase['text_sha256'],
            'actual_text_sha256': hashlib.sha256(content.encode()).hexdigest(),
            'text_matches': content == expected_content, 'rows': rows, 'errors': errors,
            'passed': not errors and all(row['passed'] for row in rows)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', type=Path, required=True)
    parser.add_argument('--commit', required=True, help='Commit/ref captured for this phase; do not substitute final HEAD for commit2 after amend.')
    parser.add_argument('--phase', choices=['commit1', 'commit2', 'amend'], required=True)
    parser.add_argument('--oracle', type=Path, default=Path(__file__).with_name('oracle.json'))
    parser.add_argument('--session-round1')
    parser.add_argument('--session-round2')
    parser.add_argument('--model-round1')
    parser.add_argument('--model-round2')
    parser.add_argument('--trace-round1', action='append', default=[])
    parser.add_argument('--trace-round2', action='append', default=[])
    parser.add_argument('--output', type=Path, help='Optional JSON evidence file; must be outside the tested repository.')
    args = parser.parse_args()
    repo = args.repo.resolve()
    if args.output and (args.output.resolve() == repo or repo in args.output.resolve().parents):
        parser.error('--output must be outside the tested repository; verification never writes Babel')
    if args.output and args.output.resolve() == args.oracle.resolve():
        parser.error('--output must not overwrite the frozen oracle')
    oracle_bytes = args.oracle.read_bytes()
    oracle = json.loads(oracle_bytes)
    bindings = {generation: {'session': getattr(args, 'session_' + generation),
                             'model': getattr(args, 'model_' + generation) or oracle['model_defaults'][generation],
                             'traces': getattr(args, 'trace_' + generation)}
                for generation in ['round1', 'round2']}
    try:
        result = verify(repo, args.commit, args.phase, oracle, bindings)
    except (EvidenceError, ValueError, KeyError, OSError, subprocess.TimeoutExpired) as error:
        result = {'schema': 'zcode-independent-line-verification/v1', 'phase': args.phase,
                  'repo': str(repo), 'passed': False, 'evidence_error': str(error)}
    result['oracle_sha256'] = hashlib.sha256(oracle_bytes).hexdigest()
    output = json.dumps(result, ensure_ascii=False, indent=2) + '\n'
    if args.output:
        args.output.write_text(output)
    sys.stdout.write(output)
    return 0 if result['passed'] else 1


if __name__ == '__main__':
    sys.exit(main())
