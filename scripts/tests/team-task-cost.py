"""Regression cases for attribution that the single-task paired run cannot exercise."""
from importlib.util import module_from_spec, spec_from_file_location
import json
from pathlib import Path
import tempfile
import unittest

spec = spec_from_file_location("team_cost", Path(__file__).resolve().parents[1] / "team-task-cost.py")
ledger = module_from_spec(spec)
spec.loader.exec_module(ledger)


def log(root, session, events):
    path = root / (session.replace("/", "~") + ".events")
    header = dict(id=session, version=15, parent=session.rpartition("/")[0] or None)
    with path.open("w") as stream:
        print(json.dumps(dict(header=header)), file=stream)
        for seq, (turn, kind, data) in enumerate(events, 1):
            print(json.dumps(dict(seq=seq, event=dict(turn=turn, kind=kind, **data))), file=stream)
    return path


def peer(sender, turn):
    return dict(origin=dict(peer=dict(**{"from": sender}, source_turn=turn)))


class LedgerTests(unittest.TestCase):
    def test_delayed_member_and_overlapping_user_input(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            lead = log(root, "a", [
                (1, "turn_start", {}), (1, "user_message", {}), (1, "turn_end", dict(stop="stopped")),
                (2, "turn_start", {}), (2, "injected", peer("a/m", 1)),
                (2, "user_message", {}), (2, "turn_end", dict(stop="cancelled")),
            ])
            log(root, "a/m", [
                (1, "turn_start", {}), (1, "injected", peer("a", 1)),
                (1, "request_header", {}), (1, "reply_checkpoint", dict(round=1, provider_call=True)),
                (1, "reply_checkpoint", dict(round=1, provider_call=True)),
                (1, "usage", dict(round=1, usage=dict(prompt=5, completion=2, cached=1))),
            ])
            data = ledger.report(lead)
            self.assertEqual(data["by_task"]["a#2"]["provider_calls_observed"], 2)
            self.assertEqual(data["by_task"]["shared"]["lead_wakeups"], 1)
            self.assertEqual(data["stop_reasons_by_task"]["shared"], {"cancelled": 1})
            self.assertEqual(data["totals"]["turns"], 3)
            self.assertEqual(data["totals"]["open_turns"], 1)
            self.assertEqual(data["totals"]["usage_records"], 1)

    def test_legacy_log_leaves_missing_evidence_visible(self):
        with tempfile.TemporaryDirectory() as directory:
            lead = log(Path(directory), "a", [
                (1, "turn_start", {}), (1, "injected", peer("a/m", None)),
                (1, "request_header", {}), (1, "reply_checkpoint", dict(round=1, reset=True)),
            ])
            cost = ledger.report(lead)["by_task"]["unattributed"]
            for key in ("peer_messages_without_source_turn", "rounds_without_call_marker",
                        "rounds_without_reported_usage"):
                self.assertEqual(cost[key], 1)
            self.assertEqual(cost["provider_calls_observed"], 0)


if __name__ == "__main__":
    unittest.main()
