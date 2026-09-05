#!/usr/bin/env python3
"""Three-protocol mock upstream for the Qoder custom-route E2E regression.

Listens on 127.0.0.1:PORT (default 9000) and serves all three upstream wire
formats QSwitch converts between:

  POST /chat/completions   -> OpenAI Chat Completions SSE
  POST /v1/messages        -> Anthropic Messages SSE
  POST /responses          -> OpenAI Responses SSE

Every request is logged as one JSON line. For safety the log records ONLY
header NAMES (never values, so no API key can leak), plus a redacted subset of
the request body.
"""
import json
import os
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = int(os.environ.get("QSWITCH_E2E_PORT", "9000"))
LOG_PATH = os.environ.get("QSWITCH_E2E_MOCK_LOG", "/tmp/qswitch-e2e-mock.log")

SENSITIVE_HEADERS = {"authorization", "x-api-key", "cookie", "proxy-authorization"}


def _sse(handler, events):
    handler.send_response(200)
    handler.send_header("Content-Type", "text/event-stream")
    handler.send_header("Cache-Control", "no-cache")
    handler.end_headers()
    for event in events:
        if event == "[DONE]":
            handler.wfile.write(b"data: [DONE]\n\n")
        else:
            name, payload = event
            if name:
                handler.wfile.write(f"event: {name}\n".encode())
            handler.wfile.write(
                f"data: {json.dumps(payload, ensure_ascii=False)}\n\n".encode()
            )
        handler.wfile.flush()
        time.sleep(0.01)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _log_req(self, body: bytes):
        try:
            parsed = json.loads(body.decode("utf-8", "replace")) if body else {}
        except json.JSONDecodeError:
            parsed = {}
        # Header NAMES only; mark whether a sensitive auth header was present.
        header_names = sorted(k.lower() for k in self.headers.keys())
        entry = {
            "time": time.strftime("%H:%M:%S"),
            "method": self.command,
            "path": self.path,
            "headerNames": header_names,
            "authHeaderPresent": {
                h: (h in header_names) for h in sorted(SENSITIVE_HEADERS)
            },
            "model": parsed.get("model"),
            "stream": parsed.get("stream"),
            "hasTools": bool(parsed.get("tools")),
            "toolChoice": parsed.get("tool_choice"),
            "reasoning": parsed.get("reasoning"),
            "maxTokens": parsed.get("max_tokens", parsed.get("max_output_tokens")),
            "hasSessionHeader": any(
                n == "x-qswitch-qoder-session-id" for n in header_names
            ),
        }
        with open(LOG_PATH, "a", encoding="utf-8") as fh:
            fh.write(json.dumps(entry, ensure_ascii=False) + "\n")

    def do_GET(self):
        self._log_req(b"")
        self._json({"object": "list", "data": [{"id": "e2e-model", "object": "model"}]})

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length) if length else b""
        self._log_req(body)
        path = self.path.split("?", 1)[0]

        if path.endswith("/v1/messages"):
            self._anthropic_sse()
        elif path.endswith("/responses"):
            self._responses_sse()
        else:
            self._chat_sse()

    def _chat_sse(self):
        chunks = [
            {"id": "cmpl-e2e", "object": "chat.completion.chunk", "model": "e2e-model",
             "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}]},
            {"id": "cmpl-e2e", "object": "chat.completion.chunk", "model": "e2e-model",
             "choices": [{"index": 0, "delta": {"content": "MOCK-CHAT"}, "finish_reason": None}]},
            {"id": "cmpl-e2e", "object": "chat.completion.chunk", "model": "e2e-model",
             "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
             "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}},
            "[DONE]",
        ]
        _sse(self, [(None, c) if c != "[DONE]" else "[DONE]" for c in chunks])

    def _anthropic_sse(self):
        events = [
            ("message_start", {"type": "message_start", "message": {
                "id": "msg_e2e", "type": "message", "role": "assistant",
                "content": [], "model": "e2e-model", "stop_reason": None,
                "usage": {"input_tokens": 1, "output_tokens": 0}}}),
            ("content_block_start", {"type": "content_block_start", "index": 0,
             "content_block": {"type": "text", "text": ""}}),
            ("content_block_delta", {"type": "content_block_delta", "index": 0,
             "delta": {"type": "text_delta", "text": "MOCK-ANTHROPIC"}}),
            ("content_block_stop", {"type": "content_block_stop", "index": 0}),
            ("message_delta", {"type": "message_delta",
             "delta": {"stop_reason": "end_turn"},
             "usage": {"output_tokens": 1}}),
            ("message_stop", {"type": "message_stop"}),
        ]
        _sse(self, events)

    def _responses_sse(self):
        events = [
            ("response.created", {"type": "response.created", "response": {"id": "resp_e2e"}}),
            ("response.output_text.delta", {"type": "response.output_text.delta", "delta": "MOCK-"}),
            ("response.output_text.delta", {"type": "response.output_text.delta", "delta": "RESPONSES"}),
            ("response.completed", {"type": "response.completed", "response": {
                "id": "resp_e2e", "status": "completed",
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}}),
        ]
        _sse(self, events)

    def _json(self, obj):
        data = json.dumps(obj, ensure_ascii=False).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
