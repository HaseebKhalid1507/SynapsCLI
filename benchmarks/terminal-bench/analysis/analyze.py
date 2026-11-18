#!/usr/bin/env python3
"""Offline TB4.1 audit. Reads recorded trials only; emits counts, never transcript text."""
import argparse
from collections import Counter
from datetime import datetime
import hashlib
import json
from pathlib import Path
import re
from statistics import median


def load(path):
    return json.loads(path.read_text())


def stamp(value):
    if isinstance(value, (int, float)):
        return value / 1000
    return datetime.fromisoformat(value.replace('Z', '+00:00')).timestamp()


def union_seconds(intervals):
    end = None
    total = 0.0
    for a, b in sorted(intervals):
        if b < a:
            raise ValueError('negative interval')
        total += max(0, b - max(a, end if end is not None else a))
        end = max(b, end if end is not None else b)
    return total


def percentile(values, p):
    if not values:
        return None
    values = sorted(values)
    return values[min(len(values) - 1, int((len(values) - 1) * p))]


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def selected_trial(jobs, task, suffix):
    matches = list((jobs / ('pilot-opus55-xhigh-daemon' + suffix)).glob(task + '__*/result.json'))
    if len(matches) != 1:
        raise ValueError(f'{task}: expected one selected trial, found {len(matches)}')
    return matches[0].parent


# Lexical proxy only: these counts are not assertions run or proof of validation quality.
CHECK = re.compile(r'\b(pytest|unittest|ctest)\b|\bcargo\s+(test|check)\b|\b(npm|pnpm|yarn)\s+(run\s+)?(test|build|lint|typecheck)\b|\b(go|uv)\s+test\b')


def trajectory_metrics(data):
    tools, models, stop = Counter(), Counter(), Counter()
    failures = timeouts = output_kills = display_truncations = checks = 0
    after = user_turns = agent_steps = 0
    max_prompt = 0
    result_count = 0
    for step in data['steps']:
        if step['source'] == 'user':
            user_turns += 1
        if step['source'] != 'agent':
            continue
        agent_steps += 1
        models[step.get('model_name', 'unknown')] += 1
        metrics = step.get('metrics') or {}
        max_prompt = max(max_prompt, metrics.get('prompt_tokens', 0) or 0)
        reason = (metrics.get('extra') or {}).get('stop_reason') or (step.get('extra') or {}).get('stop_reason')
        if reason:
            stop[reason] += 1
        for call in step.get('tool_calls') or []:
            name = call['function_name']
            tools[name] += 1
            after += int(user_turns >= 2)
            args = call.get('arguments') or {}
            if isinstance(args, dict) and name.lower() in ('bash', 'shell_send'):
                checks += bool(CHECK.search(str(args.get('command', args.get('input', '')))))
        for result in (step.get('observation') or {}).get('results', []):
            result_count += 1
            content = result.get('content', '')
            if not isinstance(content, str):
                content = json.dumps(content)
            failures += content.startswith('Tool execution failed:')
            timeouts += content.startswith('Tool execution failed: Command timed out after')
            output_kills += '[output truncated at ' in content
            display_truncations += '[truncated — ' in content or '[Output virtualised:' in content
    return dict(agent_steps=agent_steps, tool_calls=sum(tools.values()), tools=dict(tools),
                models=dict(models), stop_reasons=dict(stop), user_turns=user_turns,
                calls_after_second_user=after, tool_results=result_count,
                tool_failures=failures, shell_timeouts=timeouts, output_limit_markers=output_kills,
                display_truncation_markers=display_truncations, check_command_proxy=checks,
                max_prompt_tokens=max_prompt)


def synaps_metrics(trial):
    agent = trial / 'agent'
    data = load(agent / 'trajectory.json')
    m = trajectory_metrics(data)
    result = load(trial / 'result.json')
    timing = result['agent_execution']
    a, b = stamp(timing['started_at']), stamp(timing['finished_at'])
    api = [json.loads(line) for line in (agent / 'api-log.jsonl').read_text().splitlines() if line]
    m.update(wall_seconds=b-a, api_requests=len(api),
             api_stop_reasons=dict(Counter(r.get('stop_reason') for r in api)),
             api_retries=sum(r.get('attempt', 1) > 1 for r in api),
             ttft_ms_p50=percentile([r['ttft_ms'] for r in api if r.get('ttft_ms') is not None], .5),
             ttft_ms_p95=percentile([r['ttft_ms'] for r in api if r.get('ttft_ms') is not None], .95),
             api_cost_usd=sum(r.get('cost_usd', 0) or 0 for r in api))
    m['usage'] = dict(sum((Counter({k: v for k, v in r.get('usage', {}).items() if k != 'hit_pct'}) for r in api), Counter()))
    m['max_prompt_tokens'] = max((sum(r.get('usage', {}).get(k, 0) or 0 for k in ('input', 'cache_read', 'cache_write')) for r in api), default=0)
    api_intervals = [(max(a, stamp(r['ts']) - r['total_ms']/1000), min(b, stamp(r['ts']))) for r in api if r.get('total_ms') is not None]
    api_intervals = [(x,y) for x,y in api_intervals if y >= x]
    starts, tool_intervals = {}, []
    with (agent / 'daemon-events.jsonl').open() as handle:
        for line in handle:
            # Skip large conversation digests and token deltas before decoding.
            if '"tool_use"' not in line and '"tool_result"' not in line:
                continue
            row = json.loads(line)
            event = row.get('event', {})
            if event.get('ev') != 'stream':
                continue
            e = event.get('event', {})
            if e.get('kind') != 'llm':
                continue
            tid = e.get('tool_id')
            if e.get('llm') == 'tool_use':
                starts[tid] = stamp(row['ts'])
            elif e.get('llm') == 'tool_result' and tid in starts:
                tool_intervals.append((starts.pop(tid), stamp(row['ts'])))
    m['tool_pairs'] = len(tool_intervals)
    m['unpaired_tool_starts'] = len(starts)
    m['api_seconds'] = union_seconds(api_intervals)
    m['tool_window_seconds'] = union_seconds(tool_intervals)
    m['api_tool_overlap_seconds'] = m['api_seconds'] + m['tool_window_seconds'] - union_seconds(api_intervals + tool_intervals)
    m['tool_only_seconds'] = m['tool_window_seconds'] - m['api_tool_overlap_seconds']
    m['unattributed_seconds'] = b-a-union_seconds(api_intervals + tool_intervals)
    m['api_runtime_sessions'] = dict(Counter(r.get('runtime_session_id', 'unknown') for r in api))
    m['api_main_session_requests'] = sum(r.get('session_id') == data['session_id'] for r in api)
    episodes = {}
    for row in api:
        key = (row.get('runtime_session_id'), row.get('trace_request_id'))
        episodes.setdefault(key, []).append(row)
    m['refusal_episodes'] = [
        [{'attempt': r['attempt'], 'stop': r['stop_reason'], 'output_tokens': r.get('usage', {}).get('output', 0)} for r in rows]
        for rows in episodes.values() if any(r.get('stop_reason') == 'refusal' for r in rows)
    ]
    m['final_metrics'] = data.get('final_metrics', {})
    return m


def group(rows):
    if not rows:
        return {'n': 0}
    keys = ('wall_seconds', 'api_seconds', 'tool_window_seconds', 'tool_only_seconds', 'api_tool_overlap_seconds', 'unattributed_seconds',
            'api_requests', 'tool_calls', 'tool_failures', 'shell_timeouts',
            'output_limit_markers', 'display_truncation_markers', 'check_command_proxy',
            'max_prompt_tokens', 'api_cost_usd')
    return dict(n=len(rows), **{k: {'sum': round(sum(r[k] for r in rows), 3),
                                'median': round(median(r[k] for r in rows), 3),
                                'tasks_nonzero': sum(bool(r[k]) for r in rows)} for k in keys})


def build(args):
    published = load(args.results)
    out = {'schema': 'synaps-tb41-audit/1', 'inputs': {'results_sha256': digest(args.results)}, 'tasks': {}, 'cc_samples': {}}
    for task, record in sorted(published['tasks'].items()):
        trial = selected_trial(args.tb41 / 'jobs', task, record['synaps']['job'])
        result = load(trial / 'result.json')
        reward = result['verifier_result']['rewards']['reward']
        if reward != record['synaps']['reward']:
            raise ValueError(f'{task}: reward disagrees with published selection')
        m = synaps_metrics(trial)
        m.update(reward=reward, refused=record['synaps']['refused'],
                 trial=str(trial.relative_to(args.tb41)),
                 hashes={name: digest(trial / 'agent' / name) for name in ('api-log.jsonl', 'trajectory.json')},
                 task_checksum=result['task_checksum'],
                 cc_board_pass_rate=record['claude_code_tb40']['pass_rate'])
        out['tasks'][task] = m
    for path in sorted((args.tb41 / 'analysis/cc').glob('*.json')):
        task, outcome, _ = path.name.rsplit('.', 2)
        if task not in out['tasks']:
            raise ValueError(f'unknown CC task {task}')
        data = load(path)
        m = trajectory_metrics(data)
        m.update(sha256=digest(path), session_id=data['session_id'], selected_outcome=outcome,
                 final_metrics=data.get('final_metrics', {}))
        stamps = [stamp(s['timestamp']) for s in data['steps'] if s.get('timestamp')]
        m['transcript_span_seconds'] = max(stamps)-min(stamps) if stamps else None
        out['cc_samples'][path.name] = m
    rows = list(out['tasks'].values())
    out['groups'] = {'all': group(rows), 'pass': group([r for r in rows if r['reward']==1]),
                     'nonrefusal_fail': group([r for r in rows if r['reward']==0 and not r['refused']]),
                     'refusal': group([r for r in rows if r['refused']])}
    out['api_stop_reasons'] = dict(sum((Counter(r['api_stop_reasons']) for r in rows), Counter()))
    out['api_usage'] = dict(sum((Counter(r['usage']) for r in rows), Counter()))
    ttft = []
    for row in rows:
        path = args.tb41 / row['trial'] / 'agent/api-log.jsonl'
        for line in path.read_text().splitlines():
            record = json.loads(line)
            if record.get('ttft_ms') is not None:
                ttft.append(record['ttft_ms'])
    out['api_ttft_ms'] = {'n': len(ttft), 'median': median(ttft), 'p95': percentile(ttft, .95)}
    if args.tb4:
        prior = load(args.tb4)
        out['inputs']['tb4_sha256'] = digest(args.tb4)
        flips = Counter()
        out['tb4_overlap'] = {}
        for task in sorted(set(prior) & set(out['tasks'])):
            old, new = prior[task], out['tasks'][task]
            key = ('pass' if old['reward']==1 else 'fail') + '_to_' + ('pass' if new['reward']==1 else 'fail')
            flips[key] += 1
            out['tb4_overlap'][task] = {'old_status': old['status'], 'old_reward': old['reward'], 'new_reward': new['reward']}
        out['tb4_transition_counts'] = dict(flips)
    return out


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--tb41', type=Path, required=True)
    p.add_argument('--results', type=Path, default=Path(__file__).resolve().parents[1]/'results-2026-11-06.json')
    p.add_argument('--tb4', type=Path)
    p.add_argument('--output', type=Path, required=True)
    args = p.parse_args()
    out = build(args)
    args.output.write_text(json.dumps(out, indent=2, sort_keys=True)+'\n')
    print(json.dumps({'tasks': len(out['tasks']), 'cc_samples': len(out['cc_samples']),
                      'groups': out['groups'], 'stop_reasons': out['api_stop_reasons'],
                      'tb4_transitions': out.get('tb4_transition_counts')}, indent=2))


if __name__ == '__main__':
    main()
