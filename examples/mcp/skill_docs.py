# /// script
# requires-python = ">=3.10"
# dependencies = ["mcp>=1.12,<2"]
# ///
"""Expose one trusted skill's Python documentation lookup as an MCP text query."""

import argparse
import sys
from pathlib import Path

import anyio
from mcp.server.fastmcp import FastMCP
from mcp.types import ToolAnnotations


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "script", type=Path, help="Absolute path to the installed search_docs.py"
    )
    parser.add_argument(
        "--query-option", help="Optional named query argument, e.g. --query-option=--query"
    )
    args = parser.parse_args()
    if not args.script.is_absolute() or not args.script.is_file():
        parser.error("script must be an absolute path to an installed file")
    script = args.script.resolve()
    server = FastMCP("skill-docs")

    @server.tool(annotations=ToolAnnotations(readOnlyHint=True))
    async def search_docs(query: str) -> str:
        """Search the installed skill documentation for a keyword or phrase."""
        if not query.strip() or len(query) > 2048 or "\0" in query or query.startswith("-"):
            raise ValueError("Supply a search phrase of up to 2048 characters, without a leading dash.")
        argv = [sys.executable, "-B", "-X", "utf8", str(script)]
        if args.query_option:
            argv.append(args.query_option)
        argv.append(query)
        with anyio.fail_after(30):
            result = await anyio.run_process(argv, cwd=script.parent.parent, check=False)
        if result.returncode:
            reason = result.stderr.decode("utf-8", errors="replace")[:2048]
            raise ValueError(f"Documentation lookup failed ({result.returncode}): {reason}")
        if len(result.stdout) > 1024 * 1024:
            raise ValueError("More than 1 MiB of results. Narrow the query.")
        return result.stdout.decode("utf-8")

    server.run()


if __name__ == "__main__":
    main()
