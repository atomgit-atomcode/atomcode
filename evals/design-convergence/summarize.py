#!/usr/bin/env python3
"""Unblind completed execution/review artifacts; never infer semantic passes."""
import argparse
from collections import defaultdict
import json
from pathlib import Path
import statistics


def summarize(root):
    manifest = json.loads((root/"private-manifest.json").read_text())
    groups = defaultdict(list)
    for entry in manifest["trials"]:
        path = root/(entry["packet"]+".json")
        packet = json.loads(path.read_text()) if path.exists() else {"status": "unfinished"}
        review_path = root/"reviews"/(entry["packet"]+".json")
        review = json.loads(review_path.read_text()) if review_path.exists() else {"status": "unreviewed"}
        groups[(entry["phase"], entry["arm"])].append((entry, packet, review))
    protocols = set()
    prompt_hashes = set()
    for items in groups.values():
        for _, _, review in items:
            if review["status"] == "model_review_only":
                protocols.add(review.get("review_protocol", 1))
                if review.get("review_prompt_sha256"):
                    prompt_hashes.add(review["review_prompt_sha256"])
    if len(protocols) > 1 or len(prompt_hashes) > 1:
        raise ValueError("Mixed review protocols/prompts; keep ratings in separate reports")
    result = []
    for (phase, arm), items in sorted(groups.items()):
        calls = [r for _, packet, _ in items for r in packet.get("requests", [])]
        usage = [r.get("usage", {}) for r in calls]
        ratings = [review["review"] for _, _, review in items if review["status"] == "model_review_only"]
        flagged = [entry["packet"] for entry, _, review in items
                   if review["status"] == "model_review_only" and review["review"]["critical_failures"]]
        successful = [p for _, p, _ in items if p.get("status") == "review_required"]
        result.append({"phase": phase, "arm": arm, "planned_trials": len(items),
            "completed_trials": len(successful),
            "infrastructure_failures": sum(p.get("status") == "infrastructure_failure" for _, p, _ in items),
            "model_reviewed_trials": len(ratings),
            "review_failures": sum(r["status"] == "review_failure" for _, _, r in items),
            "unreviewed_trials": sum(r["status"] == "unreviewed" for _, _, r in items),
            "model_flagged_critical_trials": len(flagged),
            "model_flagged_packets": flagged,
            "model_unnecessary_questions": sum(r["unnecessary_questions"] for r in ratings),
            "model_unjustified_redesigns": sum(r["unjustified_redesigns"] for r in ratings),
            "model_marked_converged": sum(r["converged"] for r in ratings),
            "successful_requests": len(calls),
            "actual_models": sorted({str(r.get("model")) for r in calls}),
            "prompt_tokens": sum(u.get("prompt_tokens", 0) for u in usage),
            "cached_tokens": sum(u.get("prompt_tokens_details", {}).get("cached_tokens", 0) for u in usage),
            "completion_tokens": sum(u.get("completion_tokens", 0) for u in usage),
            "sum_request_seconds": round(sum(r["seconds"] for r in calls), 2),
            "median_trial_request_seconds": round(statistics.median(sum(r["seconds"] for r in p.get("requests", [])) for p in successful), 2) if successful else None,
            "answer_characters": sum(len(t["text"]) for _, p, _ in items for t in p.get("turns", []) if t["kind"] == "answer"),
            "human_validated_trials": 0})
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("run", type=Path)
    args = parser.parse_args()
    data = summarize(args.run)
    (args.run/"summary.json").write_text(json.dumps(data, ensure_ascii=False, indent=2)+"\n")
    print(json.dumps(data, ensure_ascii=False, indent=2))
