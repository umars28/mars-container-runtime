#!/usr/bin/env python3
"""A stand-in OTLP/HTTP collector that prints the span tree it receives.

Useful for checking what mars sends without a Tempo instance running:

    ./scripts/otlp-echo.py 4318 &
    MARS_OTLP_ENDPOINT=127.0.0.1:4318 mars run demo
"""
import http.server
import json
import sys

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 4318


class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length)

        self.send_response(200)
        self.send_header("Content-Length", "0")
        self.end_headers()

        try:
            payload = json.loads(body)
            scope = payload["resourceSpans"][0]["scopeSpans"][0]
            spans = scope["spans"]
        except (KeyError, IndexError, json.JSONDecodeError) as error:
            print(f"unparseable payload: {error}", flush=True)
            return

        root = spans[0]
        base = int(root["startTimeUnixNano"])
        total = (int(root["endTimeUnixNano"]) - base) // 1000

        print(f"trace {root['traceId']}  {len(spans)} spans  {total}us total", flush=True)

        for span in spans[1:]:
            start = int(span["startTimeUnixNano"])
            end = int(span["endTimeUnixNano"])
            offset = (start - base) // 1000
            width = (end - start) // 1000
            bar = "#" * max(1, width * 40 // max(total, 1))
            print(f"  {offset:>6}us {width:>6}us  {span['name']:<26} {bar}", flush=True)

    def log_message(self, *args):
        pass


print(f"listening on 127.0.0.1:{PORT}", flush=True)
http.server.HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
