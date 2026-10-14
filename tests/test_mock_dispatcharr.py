#!/usr/bin/env python3
"""tests/mock_dispatcharr.py answers the sign-in sequence the client makes, in the shapes it reads."""
import json
import sys
import threading
import unittest
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mock_dispatcharr  # noqa: E402


class MockDispatcharr(unittest.TestCase):
    def start(self, *argv):
        args = mock_dispatcharr.parse_args(["--port", "0", *argv])
        server = mock_dispatcharr.serve(args)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        self.base = f"http://127.0.0.1:{server.server_address[1]}"

    def call(self, method, path, headers=None, body=None):
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(self.base + path, data=data, method=method, headers=headers or {})
        try:
            with urllib.request.urlopen(req) as r:
                return r.status, json.loads(r.read())
        except urllib.error.HTTPError as e:
            return e.code, json.loads(e.read())

    def sign_in(self, user="admin", password="admin"):
        status, tokens = self.call("POST", "/api/accounts/token/", {"Content-Type": "application/json"},
                                   {"username": user, "password": password})
        return status, tokens

    def test_version_needs_no_credential(self):
        self.start()
        self.assertEqual(self.call("GET", "/api/core/version/"), (200, {"version": "0.32.0", "timestamp": None}))

    def test_wrong_password_is_401(self):
        self.start()
        self.assertEqual(self.sign_in(password="nope")[0], 401)

    def test_an_existing_key_is_returned_and_opens_the_reads(self):
        self.start("--key", "EXISTING", "--channels", "12", "--groups", "3")
        status, tokens = self.sign_in()
        self.assertEqual(status, 200)
        bearer = {"Authorization": f"Bearer {tokens['access']}"}
        self.assertEqual(self.call("GET", "/api/accounts/api-keys/", bearer), (200, {"key": "EXISTING"}))
        status, channels = self.call("GET", "/api/channels/channels/summary/", {"X-API-Key": "EXISTING"})
        self.assertEqual(status, 200)
        self.assertEqual(len(channels), 12)
        self.assertEqual(set(channels[0]), {"id", "uuid", "name", "logo_id", "channel_number", "epg_data_id", "channel_group_id"})
        self.assertTrue(any(c["channel_number"] is None for c in channels), "numberless channels are exercised")
        status, groups = self.call("GET", "/api/channels/groups/", {"X-API-Key": "EXISTING"})
        self.assertEqual((status, len(groups)), (200, 3))
        self.assertEqual(sum(g["channel_count"] for g in groups), 12)
        self.assertEqual(self.call("GET", "/api/channels/groups/", {"X-API-Key": "WRONG"})[0], 401)

    def test_generate_replaces_the_key(self):
        self.start()
        _, tokens = self.sign_in()
        bearer = {"Authorization": f"Bearer {tokens['access']}"}
        self.assertEqual(self.call("GET", "/api/accounts/api-keys/", bearer), (200, {"key": None}))
        status, generated = self.call("POST", "/api/accounts/api-keys/generate/", bearer, {})
        self.assertEqual(status, 201)
        self.assertEqual(self.call("GET", "/api/accounts/api-keys/", bearer), (200, {"key": generated["key"]}))

    def test_a_post_body_is_consumed_on_every_path(self):
        # Two POSTs with bodies on ONE keep-alive connection: an unread body would be parsed as the
        # second request line and answered 400.
        import http.client
        self.start()
        _, tokens = self.sign_in()
        conn = http.client.HTTPConnection(self.base.removeprefix("http://"))
        self.addCleanup(conn.close)
        for _ in range(2):
            conn.request("POST", "/api/accounts/api-keys/generate/", body=b"{}",
                         headers={"Authorization": f"Bearer {tokens['access']}", "Content-Type": "application/json"})
            response = conn.getresponse()
            response.read()
            self.assertEqual(response.status, 201)

    def test_the_lineup_is_deterministic(self):
        a = mock_dispatcharr.lineup(7, 30, 4)
        b = mock_dispatcharr.lineup(7, 30, 4)
        self.assertEqual(a, b)
        self.assertNotEqual(a, mock_dispatcharr.lineup(8, 30, 4))

    def test_unknown_paths_are_404(self):
        self.start()
        self.assertEqual(self.call("GET", "/api/epg/grid/")[0], 404)


if __name__ == "__main__":
    unittest.main()
