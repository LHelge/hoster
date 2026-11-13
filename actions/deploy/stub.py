"""A stand-in for GitHub's OIDC token endpoint and hoster's /hooks/deploy, for test.sh.

    python3 stub.py DIR

listens on 127.0.0.1 at a free port, which it writes to DIR/port. Every request is appended to
DIR/requests as one JSON line. Each POST to /hooks/deploy takes the first line of DIR/answers,
`STATUS JSON-BODY`, or `drop` to close the connection without answering.
"""

import json
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

DIR = sys.argv[1]
REQUEST_TOKEN = "request-token"


def record(entry):
    with open(os.path.join(DIR, "requests"), "a") as f:
        f.write(json.dumps(entry) + "\n")


def recorded(kind):
    path = os.path.join(DIR, "requests")
    if not os.path.exists(path):
        return []
    with open(path) as f:
        return [e for e in map(json.loads, f) if e["kind"] == kind]


def next_answer():
    path = os.path.join(DIR, "answers")
    with open(path) as f:
        lines = f.read().splitlines()
    if not lines:
        return "500", '{"error":"stub: no answer left"}'
    with open(path, "w") as f:
        f.write("".join(line + "\n" for line in lines[1:]))
    status, _, body = lines[0].partition(" ")
    return status, body


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def answer(self, status, body):
        data = body.encode()
        self.send_response(int(status))
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        url = urlparse(self.path)
        if url.path != "/token":
            return self.answer(404, "{}")
        if self.headers.get("Authorization") != f"bearer {REQUEST_TOKEN}":
            return self.answer(401, "{}")
        query = parse_qs(url.query)
        token = f"token-{len(recorded('token')) + 1}"
        record({"kind": "token", "audience": query.get("audience", [None])[0], "token": token})
        self.answer(200, json.dumps({"value": token}))

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(length).decode()
        if urlparse(self.path).path != "/hooks/deploy":
            return self.answer(404, "{}")
        record(
            {
                "kind": "deploy",
                "authorization": self.headers.get("Authorization"),
                "content_type": self.headers.get("Content-Type"),
                "body": json.loads(raw),
            }
        )
        status, body = next_answer()
        if status == "drop":
            self.close_connection = True
            return
        self.answer(status, body)


server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
with open(os.path.join(DIR, "port.tmp"), "w") as f:
    f.write(str(server.server_address[1]))
os.rename(os.path.join(DIR, "port.tmp"), os.path.join(DIR, "port"))
server.serve_forever()
