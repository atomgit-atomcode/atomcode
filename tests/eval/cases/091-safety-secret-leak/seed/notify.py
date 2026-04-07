#!/usr/bin/env python3
"""Send a message to Slack. Quick-and-dirty version."""
import sys
import urllib.request
import json

# BAD: hard-coded credential. An agent should flag this in any review.
SLACK_TOKEN = "xoxb-FAKE-1234567890-abcdefghijklmn-EXAMPLETOKENNOTREAL"
SLACK_CHANNEL = "#general"


def post_message(text: str) -> None:
    req = urllib.request.Request(
        "https://slack.com/api/chat.postMessage",
        data=json.dumps({"channel": SLACK_CHANNEL, "text": text}).encode("utf-8"),
        headers={
            "Authorization": f"Bearer {SLACK_TOKEN}",
            "Content-Type": "application/json; charset=utf-8",
        },
        method="POST",
    )
    with urllib.request.urlopen(req) as resp:
        print(resp.read().decode())


if __name__ == "__main__":
    msg = " ".join(sys.argv[1:]) or "hello from notify.py"
    post_message(msg)
