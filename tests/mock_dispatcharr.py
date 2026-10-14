#!/usr/bin/env python3
"""A SYNTHETIC Dispatcharr server: enough of v0.32.0's REST surface for the app to sign in, obtain
an API key and list channels, with no real lineup, account or address anywhere in it.

What it answers, with the shapes `rust-modules/dispatcharr/src/dispatcharr/models.rs` reads:

  GET  /api/core/version/                 {"version","timestamp"}, no auth (AllowAny upstream)
  POST /api/accounts/token/               {"refresh","access"} for --user/--password, else 401
  GET  /api/accounts/api-keys/            {"key": <current key or null>}            (Bearer)
  POST /api/accounts/api-keys/generate/   {"key","user"}, REPLACING the current key (Bearer)
  GET  /api/channels/channels/summary/    the generated lineup                      (X-API-Key)
  GET  /api/channels/groups/              the generated groups                      (X-API-Key)

Anything else is a 404 and a line on stderr naming it, so extending it is one branch. The lineup is
generated from --seed, never the clock: two servers started alike answer byte-identically.

    python3 tests/mock_dispatcharr.py --host <this-machine-lan-ip> --port 9192
    printf '%s\n' http://<this-machine-lan-ip>:9192 admin admin > /tmp/plxnative-dispatcharr

The default binds loopback only, like tests/mock_pms.py; pass the LAN address for a television.
"""
import argparse
import json
import random
import sys
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

VERSION = "0.32.0"
ACCESS = "mock.access.token"
REFRESH = "mock.refresh.token"


def lineup(seed, count, groups):
    """`count` channels across `groups` groups. Numbers are mostly whole, a few `N.1` sub-channels
    (the server stores a float), and every tenth channel has no number, logo or guide data."""
    rng = random.Random(seed)
    group_rows = [{"id": g + 1, "name": f"Group {g + 1}", "channel_count": 0} for g in range(groups)]
    channels = []
    number = 2.0
    for i in range(count):
        cid = i + 1
        numberless = cid % 10 == 0
        group = group_rows[rng.randrange(groups)] if groups else None
        if group:
            group["channel_count"] += 1
        channels.append({
            "id": cid,
            "uuid": str(uuid.UUID(int=rng.getrandbits(128), version=4)),
            "name": f"Channel {cid:03d}",
            "logo_id": None if numberless else 100 + cid,
            "channel_number": None if numberless else number,
            "epg_data_id": None if numberless else 1000 + cid,
            "channel_group_id": group["id"] if group else None,
        })
        number = number + 0.1 if rng.random() < 0.15 and number == int(number) else float(int(number) + 1)
        number = round(number, 1)
    rng.shuffle(channels)  # the server's order is not guide order; the client sorts
    return channels, group_rows


class State:
    def __init__(self, args):
        self.user, self.password = args.user, args.password
        self.key = args.key or None
        self.channels, self.groups = lineup(args.seed, args.channels, args.groups)


def make_handler(state):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, fmt, *a):  # one compact line per request on stderr
            sys.stderr.write("mock_dispatcharr: %s %s\n" % (self.command, self.path))

        def reply(self, status, body):
            data = json.dumps(body).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def body(self):
            length = int(self.headers.get("Content-Length") or 0)
            raw = self.rfile.read(length) if length else b""
            try:
                return json.loads(raw or b"{}")
            except ValueError:
                return None

        def bearer_ok(self):
            return self.headers.get("Authorization") == f"Bearer {ACCESS}"

        def key_ok(self):
            return state.key is not None and self.headers.get("X-API-Key") == state.key

        def path_only(self):
            return self.path.split("?", 1)[0]

        def do_GET(self):
            path = self.path_only()
            if path == "/api/core/version/":
                return self.reply(200, {"version": VERSION, "timestamp": None})
            if path == "/api/accounts/api-keys/":
                if not self.bearer_ok():
                    return self.reply(401, {"detail": "Authentication credentials were not provided."})
                return self.reply(200, {"key": state.key})
            if path in ("/api/channels/channels/summary/", "/api/channels/groups/"):
                if not self.key_ok():
                    return self.reply(401, {"detail": "Invalid API key"})
                return self.reply(200, state.channels if "summary" in path else state.groups)
            sys.stderr.write(f"mock_dispatcharr: unhandled GET {path}\n")
            return self.reply(404, {"detail": "Not found."})

        def do_POST(self):
            path = self.path_only()
            # Read the body on EVERY path: bytes left unread on a keep-alive connection are parsed
            # as the next request line.
            payload = self.body()
            if path == "/api/accounts/token/":
                creds = payload or {}
                if creds.get("username") == state.user and creds.get("password") == state.password:
                    return self.reply(200, {"refresh": REFRESH, "access": ACCESS})
                return self.reply(401, {"detail": "No active account found with the given credentials"})
            if path == "/api/accounts/api-keys/generate/":
                if not self.bearer_ok():
                    return self.reply(401, {"detail": "Authentication credentials were not provided."})
                state.key = uuid.uuid4().hex
                return self.reply(201, {"key": state.key, "user": {"id": 1, "username": state.user}})
            sys.stderr.write(f"mock_dispatcharr: unhandled POST {path}\n")
            return self.reply(404, {"detail": "Not found."})

    return Handler


def parse_args(argv=None):
    p = argparse.ArgumentParser(description="Synthetic Dispatcharr v0.32.0 for the app's sign-in and channel reads.")
    p.add_argument("--host", default="127.0.0.1")
    p.add_argument("--port", type=int, default=9192)
    p.add_argument("--user", default="admin")
    p.add_argument("--password", default="admin")
    p.add_argument("--key", default="", help="the user's existing API key (default: none, so sign-in generates one)")
    p.add_argument("--channels", type=int, default=60)
    p.add_argument("--groups", type=int, default=4)
    p.add_argument("--seed", type=int, default=1)
    return p.parse_args(argv)


def serve(args):
    server = ThreadingHTTPServer((args.host, args.port), make_handler(State(args)))
    return server


def main():
    args = parse_args()
    server = serve(args)
    print(f"mock_dispatcharr: http://{args.host}:{server.server_address[1]} user={args.user} "
          f"channels={args.channels} groups={args.groups}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
