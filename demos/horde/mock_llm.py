#!/usr/bin/env python3
"""Deterministic mock of the agentgateway LLM listener for the horde demo.

Speaks just enough of the Anthropic Messages streaming API for revenant-llm to
parse a completed text turn. Every request returns the same marker text, so an
eval is free, offline, and identical on every peer — which is exactly what lets
three independent revenants REPRODUCE the same molt and reach a quorum.

No model, no keys, no cost. POST /v1/messages -> SSE.
"""
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MARKER = "HORDE_REPRODUCE_OK"


def sse(event, data):
    return f"event: {event}\ndata: {json.dumps(data)}\n\n".encode()


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):  # quiet
        pass

    def do_GET(self):
        # health / readiness pokes
        self.send_response(200)
        self.send_header("content-type", "text/plain")
        self.end_headers()
        self.wfile.write(b"mock-llm ok")

    def do_POST(self):
        length = int(self.headers.get("content-length", 0))
        _ = self.rfile.read(length)  # ignore the request body
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()
        text = f"{MARKER}: reproduced the molt deterministically."
        for chunk in (
            sse("message_start", {"type": "message_start", "message": {
                "model": "mock-1", "usage": {"input_tokens": 1, "output_tokens": 0}}}),
            sse("content_block_start", {"type": "content_block_start", "index": 0,
                "content_block": {"type": "text", "text": ""}}),
            sse("content_block_delta", {"type": "content_block_delta", "index": 0,
                "delta": {"type": "text_delta", "text": text}}),
            sse("content_block_stop", {"type": "content_block_stop", "index": 0}),
            sse("message_delta", {"type": "message_delta",
                "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 12}}),
            sse("message_stop", {"type": "message_stop"}),
        ):
            self.wfile.write(chunk)
        self.wfile.flush()


if __name__ == "__main__":
    import os
    port = int(os.environ.get("MOCK_PORT", "9000"))
    ThreadingHTTPServer(("0.0.0.0", port), Handler).serve_forever()
