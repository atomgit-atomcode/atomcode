#!/usr/bin/env python3
"""Minimal MCP server that ships with AtomCode as a worked example.

Why this file exists: a fresh phone install shipped an empty `/mcp/status` and a
skill that explains MCP in prose, which is not the same as a server the agent can
actually call. This is the smallest thing that is genuinely an MCP server, and it
was written for what the phone can run — the bundled `python3` (RustPython), so it
uses the standard library only and stays compatible with a partial interpreter:

  * no third-party imports, no `pip`, no network
  * no `hashlib`/`ssl`/`ctypes` reliance (those are not guaranteed in this build)
  * `%`-free string building where formatting could differ, `.format()` elsewhere

Transport: MCP "stdio" — newline-delimited JSON-RPC 2.0, one message per line, on
stdin/stdout. stdout must carry protocol messages ONLY; diagnostics go to stderr,
because one stray print on stdout breaks the handshake.

The two tools are deliberately read-only (annotations.readOnlyHint), so plan mode
and read-only callers are allowed to use them.
"""

import json
import sys
import time

PROTOCOL_VERSION = "2025-11-25"
SERVER_NAME = "example-tools"
SERVER_VERSION = "1.0.0"

TOOLS = [
    {
        "name": "now",
        "description": (
            "Current date and time on this device. Useful for timestamps in "
            "filenames, logs and reports."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {},
            "additionalProperties": False,
        },
        "annotations": {"readOnlyHint": True},
    },
    {
        "name": "text_stats",
        "description": (
            "Count lines, words and characters in the given text. Useful to size a "
            "document before reading or editing it."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "text": {"type": "string", "description": "Text to measure."}
            },
            "required": ["text"],
            "additionalProperties": False,
        },
        "annotations": {"readOnlyHint": True},
    },
]

INSTRUCTIONS = (
    "Example server bundled with AtomCode. It exposes read-only helpers "
    "(now, text_stats) to demonstrate the MCP wiring; delete its entry from "
    "mcp.json when you add servers of your own."
)


class RpcError(Exception):
    def __init__(self, code, message):
        Exception.__init__(self, message)
        self.code = code
        self.message = message


def emit(message):
    """Write one protocol message. Never print to stdout any other way."""
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def tool_now(_arguments):
    stamp = time.strftime("%Y-%m-%d %H:%M:%S")
    epoch = int(time.time())
    return "{0} (device local time, unix {1})".format(stamp, epoch)


def tool_text_stats(arguments):
    text = arguments.get("text")
    if not isinstance(text, str):
        raise RpcError(-32602, "text_stats requires a string 'text' argument")
    lines = len(text.splitlines())
    words = len(text.split())
    return "lines: {0}\nwords: {1}\ncharacters: {2}".format(lines, words, len(text))


TOOL_HANDLERS = {"now": tool_now, "text_stats": tool_text_stats}


def call_tool(params):
    name = params.get("name")
    handler = TOOL_HANDLERS.get(name)
    if handler is None:
        raise RpcError(-32602, "unknown tool: {0}".format(name))
    arguments = params.get("arguments") or {}
    if not isinstance(arguments, dict):
        raise RpcError(-32602, "tool arguments must be an object")
    try:
        text = handler(arguments)
    except RpcError as error:
        return {
            "content": [{"type": "text", "text": error.message}],
            "isError": True,
        }
    except Exception as error:  # a tool failure is data, not a transport failure
        return {"content": [{"type": "text", "text": str(error)}], "isError": True}
    return {"content": [{"type": "text", "text": text}], "isError": False}


def handle(method, params):
    if method == "initialize":
        return {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
            "instructions": INSTRUCTIONS,
        }
    if method == "tools/list":
        return {"tools": TOOLS}
    if method == "tools/call":
        return call_tool(params)
    if method == "ping":
        return {}
    # Known-but-unimplemented methods get an empty result so the client's
    # capability probe does not report the server as broken.
    if method in ("resources/list", "prompts/list"):
        return {"resources": [], "prompts": []}
    raise RpcError(-32601, "method not found: {0}".format(method))


def main():
    for raw in sys.stdin:
        line = raw.strip()
        if not line:
            continue
        try:
            request = json.loads(line)
        except Exception as error:
            emit(
                {
                    "jsonrpc": "2.0",
                    "id": None,
                    "error": {"code": -32700, "message": "parse error: {0}".format(error)},
                }
            )
            continue
        if not isinstance(request, dict):
            continue
        is_notification = "id" not in request
        request_id = request.get("id")
        method = request.get("method")
        params = request.get("params") or {}
        if not isinstance(params, dict):
            params = {}
        try:
            result = handle(method, params)
        except RpcError as error:
            if not is_notification:
                emit(
                    {
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "error": {"code": error.code, "message": error.message},
                    }
                )
            continue
        except Exception as error:
            sys.stderr.write("example-tools: {0}\n".format(error))
            sys.stderr.flush()
            if not is_notification:
                emit(
                    {
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "error": {"code": -32603, "message": str(error)},
                    }
                )
            continue
        if not is_notification:
            emit({"jsonrpc": "2.0", "id": request_id, "result": result})


if __name__ == "__main__":
    main()
