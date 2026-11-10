"""Offline extractor invariants; no model or benchmark execution."""
import json
from pathlib import Path
import tempfile
import unittest

from analyze import selected_trial, stamp, trajectory_metrics, union_seconds


class ExtractorTests(unittest.TestCase):
    def test_union_does_not_double_count_parallel_work(self):
        self.assertEqual(union_seconds([(3, 8), (0, 5), (10, 12)]), 10)
        self.assertEqual(union_seconds([]), 0)
        with self.assertRaises(ValueError):
            union_seconds([(2, 1)])

    def test_timestamps(self):
        # datetime keeps microseconds, so nanoseconds are truncated on input.
        self.assertAlmostEqual(stamp('2026-11-06T04:53:30.980827991Z'), 1793940810.980828, delta=0.000002)
        self.assertEqual(stamp(1793940810983), 1793940810.983)

    def test_selection_never_chooses_best_reward(self):
        with tempfile.TemporaryDirectory() as tmp:
            jobs = Path(tmp)
            base = jobs / 'pilot-opus55-xhigh-daemon-rest2'
            trial = base / 'task__one'
            trial.mkdir(parents=True)
            (trial / 'result.json').write_text('{}')
            self.assertEqual(selected_trial(jobs, 'task', '-rest2'), trial)
            duplicate = base / 'task__two'
            duplicate.mkdir()
            (duplicate / 'result.json').write_text('{}')
            with self.assertRaises(ValueError):
                selected_trial(jobs, 'task', '-rest2')
            with self.assertRaises(ValueError):
                selected_trial(jobs, 'absent', '-rest2')

    def test_calls_are_not_claimed_executed_without_results(self):
        data = {'steps': [
            {'source': 'user'},
            {'source': 'agent', 'model_name': 'model', 'tool_calls': [
                {'function_name': 'bash', 'arguments': {'command': 'pytest -q'}}]},
            {'source': 'user'},
            {'source': 'agent', 'model_name': 'model', 'tool_calls': [
                {'function_name': 'read', 'arguments': {}}],
             'observation': {'results': [{'content': 'Tool execution failed: Command timed out after 30s'}]}},
        ]}
        m = trajectory_metrics(data)
        self.assertEqual(m['tool_calls'], 2)
        self.assertEqual(m['tool_results'], 1)
        self.assertEqual(m['calls_after_second_user'], 1)
        self.assertEqual(m['check_command_proxy'], 1)
        self.assertEqual(m['shell_timeouts'], 1)

    def test_output_limits_are_distinct_from_display_spills(self):
        m = trajectory_metrics({'steps': [{'source': 'agent', 'observation': {'results': [
            {'content': '[output truncated at 100]'},
            {'content': '[truncated — 100000 total bytes, showing 500]'},
        ]}}]})
        self.assertEqual(m['output_limit_markers'], 1)
        self.assertEqual(m['display_truncation_markers'], 1)

    def test_committed_dataset_reconciles(self):
        data = json.loads(Path(__file__).with_name('metrics.json').read_text())
        tasks = data['tasks']
        self.assertEqual(len(tasks), 63)
        self.assertEqual(sum(r['reward'] == 1 for r in tasks.values()), 39)
        self.assertEqual(len(data['cc_samples']), 84)
        for row in tasks.values():
            self.assertAlmostEqual(row['wall_seconds'], row['api_seconds'] + row['tool_only_seconds'] + row['unattributed_seconds'], places=3)
            self.assertEqual(row['tool_pairs'], row['tool_results'])
            self.assertEqual(sum(row['api_stop_reasons'].values()), row['api_requests'])
        self.assertEqual(sum(data['tb4_transition_counts'].values()), 57)
        self.assertEqual(sum(r['api_stop_reasons'].get('refusal', 0) for r in tasks.values()), 24)
        mismatch = [task for task, r in tasks.items() if r['api_requests'] != r['agent_steps']]
        self.assertEqual(mismatch, ['react-lead-form'])


if __name__ == '__main__':
    unittest.main()
