import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("design_eval", Path(__file__).with_name("eval.py"))
e = importlib.util.module_from_spec(spec)
spec.loader.exec_module(e)


class FakeClient:
    def __init__(self):
        self.calls = []

    def complete(self, messages):
        self.calls.append([dict(message) for message in messages])
        return {"role": "assistant", "content": "TubeSeq 是试管序号", "reasoning_content": "private thinking"}, {"model": "fake", "usage": {}, "seconds": 0}


class EvalTests(unittest.TestCase):
    def test_openrouter_preset_and_explicit_endpoint_override(self):
        from tempfile import TemporaryDirectory
        with TemporaryDirectory() as directory:
            path = Path(directory)/'config.toml'
            base = '[providers.eval]\ntype="openrouter"\nmodel="example"\napi_key="fake"\n'
            path.write_text(base)
            endpoint, _, _ = e.configured_provider(path, 'eval')
            self.assertEqual(endpoint, 'https://openrouter.ai/api/v1/chat/completions')
            path.write_text(base+'base_url="https://example.invalid/custom/"\n')
            endpoint, _, _ = e.configured_provider(path, 'eval')
            self.assertEqual(endpoint, 'https://example.invalid/custom/chat/completions')

    def test_extracts_rust_multiline_and_escaped_literals(self):
        source = 'const X: &str = "\\\n  first\\n\\\n    second\\\"quoted\\\"\nthird";'
        self.assertEqual(e.rust_constant(source, "X"), 'first\nsecond"quoted"\nthird')
        with self.assertRaises(ValueError):
            e.rust_constant(source, "missing")

    def test_ordinary_history_retains_all_prior_turns_and_endpoint_reasoning(self):
        client = FakeClient()
        result = e.trial(client, {"persona": "system"}, {"turns": ["one", "two", "three", "four"]}, False)
        self.assertEqual(len(client.calls), 4)
        self.assertEqual([m["content"] for m in client.calls[-1] if m["role"] == "user"], ["one", "two", "three", "four"])
        self.assertIn("reasoning_content", client.calls[-1][2])
        self.assertNotIn("private thinking", json.dumps(result))
        self.assertEqual(result["status"], "review_required")

    def test_loaded_skill_is_exposed_once_as_a_tool_result(self):
        client = FakeClient()
        e.trial(client, {"persona": "system", "loaded_skill": "ORIGINAL SKILL"},
                {"turns": ["local revision", "keep going"]}, False)
        first = client.calls[0]
        self.assertEqual(first[1]["content"], "local revision")
        self.assertEqual(first[2]["tool_calls"][0]["function"]["name"], "use_skill")
        self.assertEqual(first[3], {"role": "tool", "tool_call_id": "eval_loaded_skill", "content": "ORIGINAL SKILL"})
        self.assertEqual(sum(m.get("role") == "tool" for m in client.calls[-1]), 1)
        plain = FakeClient()
        e.trial(plain, {"persona": "system"}, {"turns": ["local revision"]}, False)
        self.assertEqual([m["role"] for m in plain.calls[0]], ["system", "user"])

    def test_summary_is_a_real_separate_request_and_replaces_the_prefix(self):
        client = FakeClient()
        prompts = {"persona": "system", "summary_system": "summarize", "summary_template": "template", "summary_framing": "historical"}
        result = e.trial(client, prompts, {"turns": ["one", "two", "three", "four", "five"]}, True)
        self.assertEqual(len(client.calls), 6)
        self.assertEqual(client.calls[3][0]["content"], "summarize")
        self.assertIn("USER: one", client.calls[3][1]["content"])
        self.assertEqual(client.calls[4][1]["content"], "historical\nTubeSeq 是试管序号")
        self.assertEqual(client.calls[4][-1]["content"], "four")
        self.assertEqual(sum(r["kind"] == "summary" for r in result["requests"]), 1)

    def test_empty_and_truncated_responses_do_not_count_as_completed_trials(self):
        for text, finish in [("partial", "length"), ("", "stop")]:
            raw = {"choices": [{"message": {"content": text}, "finish_reason": finish}]}
            class Response:
                def __enter__(self):
                    import io
                    return io.StringIO(json.dumps(raw))
                def __exit__(self, *args):
                    pass
            client = e.Client("https://example.invalid/chat/completions", "secret", "fake", None, 100, 1)
            with patch.object(client.opener, "open", return_value=Response()):
                with self.assertRaisesRegex(RuntimeError, "incomplete_or_empty_response"):
                    client.complete([])

    def test_ablation_cases_have_explicit_local_change_and_unknown_facts(self):
        cases = json.loads(Path(__file__).with_name("ablation-cases.json").read_text())
        self.assertEqual(len(cases), 3)
        for case in cases:
            self.assertEqual(len(case["turns"]), 6)
            self.assertIn("Duration 不存列", case["turns"][0])
            self.assertIn("没有确认 TubeSeq", case["turns"][0])
        evidence = next(c for c in cases if c["id"] == "meaning-and-new-evidence")
        self.assertNotIn("BootId", "".join(evidence["turns"][:3]))
        self.assertIn("BootId", evidence["turns"][3])

    def test_cases_cover_both_preservation_and_valid_revision(self):
        cases = json.loads(Path(__file__).with_name("cases.json").read_text())
        self.assertEqual(len(cases), 6)
        self.assertTrue(all(len(case["turns"]) == 6 for case in cases))
        self.assertIn("BootId", next(c for c in cases if c["id"] == "new-evidence")["rubric"])
        self.assertIn("只交付", next(c for c in cases if c["id"] == "changed-scope")["rubric"])

class ReviewValidationTests(unittest.TestCase):
    def test_fabricated_quotes_and_invalid_counts_are_rejected(self):
        spec = importlib.util.spec_from_file_location("design_review", Path(__file__).with_name("review.py"))
        r = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(r)
        packet = {"turns": [{"kind": "answer", "text": "TubeSeq 是试管序号"}]}
        review = {"critical_failures": [], "unnecessary_questions": 0, "unjustified_redesigns": 0, "converged": True, "notes": ""}
        self.assertEqual(r.validate(review, packet), review)
        with self.assertRaises(ValueError):
            r.validate({**review, "unnecessary_questions": True}, packet)
        with self.assertRaises(ValueError):
            r.validate({**review, "critical_failures": [{"turn": 1, "quote": "设备序号", "reason": "made up"}]}, packet)

    def test_future_requirements_and_assistant_claims_are_not_user_evidence(self):
        spec = importlib.util.spec_from_file_location("design_review_v2", Path(__file__).with_name("review.py"))
        r = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(r)
        packet = {"turns": [
            {"kind": "answer", "user": "先讨论方案", "text": "先按 event_id 去重"},
            {"kind": "summary", "text": "此前按 event_id 去重"},
            {"kind": "answer", "user": "现在必须保留每次重复投递", "text": "仍按 event_id 去重"}]}
        base = {"unnecessary_questions": 0, "unjustified_redesigns": 0, "converged": False, "notes": ""}
        def rating(turn, quote, req_turn, req_quote):
            return {**base, "critical_failures": [{"turn": turn, "quote": quote,
                "requirement_turn": req_turn, "requirement_quote": req_quote, "reason": "discarding required deliveries"}]}
        valid = rating(2, "仍按 event_id 去重", 2, "必须保留每次重复投递")
        self.assertEqual(r.validate(valid, packet), valid)
        for bad in [rating(1, "先按 event_id 去重", 2, "必须保留每次重复投递"),
                    rating(0, "此前按 event_id 去重", 2, "必须保留每次重复投递"),
                    rating(2, "仍按 event_id 去重", 1, "先按 event_id 去重"),
                    rating(2, "仍按 event_id 去重", True, "先讨论方案")]:
            with self.assertRaises(ValueError):
                r.validate(bad, packet)

    def test_repeated_summary_quotes_require_unambiguous_source(self):
        spec = importlib.util.spec_from_file_location("design_review_checkpoint", Path(__file__).with_name("review.py"))
        r = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(r)
        packet = {"turns": [
            {"kind": "answer", "user": "先讨论", "text": "first"},
            {"kind": "summary", "text": "按 event_id 去重"},
            {"kind": "answer", "user": "必须保留每次重复投递", "text": "second"},
            {"kind": "summary", "text": "按 event_id 去重"}]}
        review = {"critical_failures": [{"turn": 0, "quote": "按 event_id 去重", "requirement_turn": 2,
            "requirement_quote": "必须保留每次重复投递", "reason": "ambiguous checkpoint"}],
            "unnecessary_questions": 0, "unjustified_redesigns": 0, "converged": False, "notes": ""}
        with self.assertRaisesRegex(ValueError, "unique checkpoint"):
            r.validate(review, packet)


class AggregateTests(unittest.TestCase):
    def test_missing_reviews_and_failed_trials_are_never_semantic_passes(self):
        import tempfile
        spec = importlib.util.spec_from_file_location("design_summarize", Path(__file__).with_name("summarize.py"))
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root/"private-manifest.json").write_text(json.dumps({"trials": [
                {"packet": "trial-000", "phase": "ordinary", "arm": "baseline"},
                {"packet": "trial-001", "phase": "ordinary", "arm": "baseline"}]}))
            (root/"trial-000.json").write_text(json.dumps({"status": "review_required", "requests": [], "turns": []}))
            (root/"trial-001.json").write_text(json.dumps({"status": "infrastructure_failure", "requests": [], "turns": []}))
            row = module.summarize(root)[0]
            self.assertEqual(row["completed_trials"], 1)
            self.assertEqual(row["infrastructure_failures"], 1)
            self.assertEqual(row["model_reviewed_trials"], 0)
            self.assertEqual(row["human_validated_trials"], 0)

    def test_mixed_review_protocols_or_prompts_cannot_be_aggregated(self):
        import tempfile
        spec = importlib.util.spec_from_file_location("design_summarize_mixed", Path(__file__).with_name("summarize.py"))
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root/"reviews").mkdir()
            (root/"private-manifest.json").write_text(json.dumps({"trials": [
                {"packet": "trial-000", "phase": "ordinary", "arm": "baseline"},
                {"packet": "trial-001", "phase": "ordinary", "arm": "candidate"}]}))
            for identity in ["trial-000", "trial-001"]:
                (root/(identity+".json")).write_text(json.dumps({"status": "review_required", "requests": [], "turns": []}))
            pairs = [({"review_protocol": 1}, {"review_protocol": 2}),
                     ({"review_protocol": 2, "review_prompt_sha256": "first"},
                      {"review_protocol": 2, "review_prompt_sha256": "second"})]
            for first, second in pairs:
                for identity, metadata in zip(["trial-000", "trial-001"], [first, second]):
                    (root/"reviews"/(identity+".json")).write_text(json.dumps({"status": "model_review_only", "review": {}, **metadata}))
                with self.assertRaisesRegex(ValueError, "Mixed review"):
                    module.summarize(root)


if __name__ == "__main__":
    unittest.main()


class NativeDriverTests(unittest.TestCase):
    def test_native_trace_excludes_reasoning_and_requires_terminal(self):
        spec = importlib.util.spec_from_file_location("native_eval", Path(__file__).with_name("native_eval.py"))
        native = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(native)
        lines = "\n".join(["startup text", "[]", json.dumps({"type": "reasoning.delta", "text": "private thinking"}),
                           json.dumps({"type": "message.delta", "text": "secret-key"})])
        events = native.visible_events(lines, "secret-key")
        self.assertEqual(events, [{"type": "message.delta", "text": "[REDACTED]"}])
        self.assertFalse(native.completed(events))
        self.assertFalse(native.completed([{"type": "turn.completed", "exit_code": 1}]))
        self.assertTrue(native.completed([{"type": "turn.completed", "exit_code": 0}]))


class CrossAgentDriverTests(unittest.TestCase):
    def load_driver(self):
        spec = importlib.util.spec_from_file_location('cross_agents', Path(__file__).with_name('cross_agents.py'))
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        return module

    def test_cross_trace_filters_nested_reasoning_and_uses_native_terminal(self):
        cross = self.load_driver()
        event = {'type':'message_end','message':{'role':'assistant','reasoning_content':'private',
            'content':[{'type':'thinking','thinking':'private'},{'type':'text','text':'visible'}]}}
        cleaned = cross.sanitize(event)
        self.assertNotIn('private', json.dumps(cleaned))
        self.assertEqual(cross.answer('omp',[cleaned,{'type':'agent_end','messages':[cleaned['message']]}]),'visible')
        self.assertFalse(cross.terminal_ok('omp',[cleaned]))
        failed = {'type':'message_end','message':{'role':'assistant','stopReason':'length'}}
        self.assertFalse(cross.terminal_ok('omp',[failed,{'type':'agent_end'}]))
        for agent,event in [('codex',{'type':'turn.completed'}),
            ('atomcode',{'type':'turn.completed','exit_code':0}),
            ('opencode',{'type':'step_finish','part':{'reason':'stop'}}),
            ('dsh',{'type':'final','text':'done'})]:
            self.assertTrue(cross.terminal_ok(agent,[event]))
            self.assertFalse(cross.terminal_ok(agent,[]))
