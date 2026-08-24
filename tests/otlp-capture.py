#!/usr/bin/env python3
"""Accept exactly one OTLP/HTTP export and write a summary the test suite can read.

    otlp-capture.py <port> <out-file>

Writes three lines: span count, trace id, and the span names separated by spaces.
"""
import http.server
import json
import sys

port = int(sys.argv[1])
out = sys.argv[2]


class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        self.send_response(200)
        self.send_header("Content-Length", "0")
        self.end_headers()

        payload = json.loads(body)
        spans = payload["resourceSpans"][0]["scopeSpans"][0]["spans"]
        names = " ".join(span["name"] for span in spans)

        with open(out, "w") as handle:
            handle.write(f"{len(spans)}\n{spans[0]['traceId']}\n{names}\n")

    def log_message(self, *args):
        pass


server = http.server.HTTPServer(("127.0.0.1", port), Handler)
server.timeout = 30
server.handle_request()
