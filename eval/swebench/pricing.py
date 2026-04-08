#!/usr/bin/env python3
"""Provider → USD cost estimation for SWE-bench eval runs.

Hardcoded price dict keyed by provider name. Unknown providers fall
back to siliconflow pricing (the cheapest) with a stderr warning.

Usage:
    python3 pricing.py <provider> <prompt_tokens> <completion_tokens>

Prints the cost in USD with 6 decimal places (one line, no trailing text),
so callers can AWK / shell-assign the result directly.
"""
import sys

# USD per 1M tokens. Update when providers change their pricing.
PRICING = {
    "siliconflow":         {"prompt": 0.14, "completion": 0.28},
    "kimi":                {"prompt": 0.20, "completion": 0.50},
    "anthropic-sonnet-4-6": {"prompt": 3.00, "completion": 15.00},
    "anthropic-haiku-4-5":  {"prompt": 0.80, "completion": 4.00},
    "anthropic-opus-4-6":   {"prompt": 15.00, "completion": 75.00},
}

_FALLBACK = "siliconflow"


def estimate_cost(provider: str, prompt_tokens: int, completion_tokens: int) -> float:
    """Pure cost-estimation function. Unknown provider → fallback (no warning here).
    The CLI entry point prints warnings to stderr; this function stays pure so it
    can be imported by tests without capturing stderr."""
    rates = PRICING.get(provider, PRICING[_FALLBACK])
    return (
        (prompt_tokens / 1_000_000.0) * rates["prompt"]
        + (completion_tokens / 1_000_000.0) * rates["completion"]
    )


def main() -> int:
    if len(sys.argv) != 4:
        print("usage: pricing.py <provider> <prompt_tokens> <completion_tokens>", file=sys.stderr)
        return 2
    provider = sys.argv[1]
    try:
        prompt_tokens = int(sys.argv[2])
        completion_tokens = int(sys.argv[3])
    except ValueError as e:
        print(f"error: invalid token count: {e}", file=sys.stderr)
        return 2
    if prompt_tokens < 0 or completion_tokens < 0:
        print("error: token counts must be >= 0", file=sys.stderr)
        return 2
    if provider not in PRICING:
        print(
            f"warning: unknown provider '{provider}', falling back to {_FALLBACK} pricing",
            file=sys.stderr,
        )
    cost = estimate_cost(provider, prompt_tokens, completion_tokens)
    print(f"{cost:.6f}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
