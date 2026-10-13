#!/usr/bin/env python3
"""Tiny stdio MCP server used by the contract-mcp example."""

import json
import sys


def read_msg():
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            raise SystemExit(0)
        line = line.decode("utf-8").strip()
        if line == "":
            break
        if line.lower().startswith("content-length:"):
            length = int(line.split(":", 1)[1].strip())
    if length is None:
        raise RuntimeError("missing Content-Length")
    return json.loads(sys.stdin.buffer.read(length).decode("utf-8"))


def write_msg(msg):
    body = json.dumps(msg).encode("utf-8")
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode("ascii"))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()


while True:
    msg = read_msg()
    if "id" not in msg:
        continue
    method = msg.get("method")
    if method == "initialize":
        write_msg(
            {
                "jsonrpc": "2.0",
                "id": msg["id"],
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "fake", "version": "1.0.0"},
                },
            }
        )
    elif method == "tools/list":
        write_msg(
            {
                "jsonrpc": "2.0",
                "id": msg["id"],
                "result": {
                    "tools": [
                        {
                            "name": "echo",
                            "description": "Echo a message",
                            "inputSchema": {
                                "type": "object",
                                "properties": {"message": {"type": "string"}},
                                "required": ["message"],
                            },
                        }
                    ]
                },
            }
        )
    elif method == "tools/call":
        args = msg.get("params", {}).get("arguments", {})
        text = "echo: " + str(args.get("message", "hello from mcp"))
        write_msg(
            {
                "jsonrpc": "2.0",
                "id": msg["id"],
                "result": {
                    "content": [{"type": "text", "text": text}],
                    "isError": False,
                },
            }
        )
    else:
        write_msg(
            {
                "jsonrpc": "2.0",
                "id": msg["id"],
                "error": {"code": -32601, "message": "unknown method"},
            }
        )
