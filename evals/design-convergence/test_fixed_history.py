import copy
import unittest

import reproduce_fixed_history as r


class FixedHistoryTests(unittest.TestCase):
    def test_skill_control_changes_only_catalog_and_replayed_tool_result(self):
        case = r.scope_fixtures()[1]
        before = copy.deepcopy(case)
        loaded = r.messages_for('PERSONA', 'SKILL BODY', case)
        plain = r.messages_for('PERSONA', '', case)
        self.assertEqual(loaded[0]['content'].split('\n===')[0], plain[0]['content'])
        self.assertEqual(loaded[1], plain[1])
        self.assertEqual(loaded[4:], plain[2:])
        self.assertEqual(loaded[3]['content'], 'SKILL BODY')
        self.assertFalse(any('tool_calls' in message or message['role'] == 'tool' for message in plain))
        self.assertEqual(case, before)

    def test_new_protocol_evidence_does_not_leak_into_prior_history(self):
        for case in r.scope_fixtures():
            history = ' '.join(message['content'] for message in case['history'])
            boot = 'EpochId' if case['id'].startswith('renamed') else 'BootId'
            self.assertNotIn(boot, history)
            self.assertIn(boot, case['user'])

    def test_renamed_control_preserves_roles_and_business_history(self):
        _, original, renamed = r.scope_fixtures()
        replacements = {'DeviceNo': 'StationId', 'PipingNo': 'LaneId', 'TubeSeq': 'ItemSeq', 'BootId': 'EpochId'}
        for old, new in zip(original['history'], renamed['history'], strict=True):
            text = old['content']
            for source, target in replacements.items():
                text = text.replace(source, target)
            self.assertEqual(new, {'role': old['role'], 'content': text})


if __name__ == '__main__':
    unittest.main()
