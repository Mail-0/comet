import base64
import json
import struct
import threading
import unittest
from urllib.error import HTTPError
from urllib.request import Request, urlopen

from keiki_server import Fixture, Handler, ThreadingHTTPServer


class KeikiFixtureTests(unittest.TestCase):
    def setUp(self):
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.fixture = Fixture()
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = "http://127.0.0.1:" + str(self.server.server_port)

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()

    def request(self, path, method="GET", body=None, public=False, raw=False):
        headers = {} if public else {"Authorization": "Bearer fixture-access"}
        if body is not None:
            headers["Content-Type"] = "application/json"
        request = Request(self.url + path, data=None if body is None else json.dumps(body).encode(), headers=headers, method=method)
        with urlopen(request, timeout=5) as response:
            data = response.read()
            return data if raw else json.loads(data)

    def test_actions_roster_templates_and_task_cancellation(self):
        path = "/api/webapp/conversations/contact:nova:operator"
        self.assertTrue(self.request(path + "/block", "POST", {})["blocked"])
        self.assertTrue(self.request(path)["blocked"])
        self.assertFalse(self.request(path + "/block", "DELETE")["blocked"])
        self.assertIn("startedAt", self.request(path + "/takeover", "POST", {})["takeover"])
        self.assertIsNone(self.request(path + "/takeover", "DELETE")["takeover"])
        self.assertTrue(self.request("/api/webapp/conversations/agent:nova:fern/end", "POST", {})["ended"])
        roster = self.request("/api/webapp/agent-groups/design/roster")
        self.assertEqual(roster["members"][0]["role"], "hub")
        self.assertEqual(roster["totals"]["backgroundTasks"], 1)
        self.assertTrue(self.request("/api/webapp/subagent-tasks/task-layout/cancel", "POST", {})["cancelled"])
        self.assertEqual(self.request("/api/webapp/conversations/api:nova:release")["tasks"][0]["status"], "cancelled")
        self.assertEqual(self.request("/api/webapp/agent-templates")["templates"][0]["id"], "research")
        agent = self.request("/api/webapp/agents", "POST", {"template": "research", "name": "Sage"})
        self.assertTrue(agent["ok"])
        self.assertTrue(any(row["id"] == agent["id"] and row["name"] == "Sage" for row in self.request("/api/webapp/agents")["agents"]))
        self.assertIn("threadsEnded", self.request("/api/webapp/agent-groups/design/end", "POST", {}))

    def test_public_avatar_and_engine_only_consent_contract(self):
        image = self.request("/api/public/agent-avatars/nova.gif?size=64&state=idle&theme=dark", public=True, raw=True)
        self.assertEqual(image[:6], b"GIF89a")
        self.assertEqual(struct.unpack("<HH", image[6:10]), (64, 64))
        preset = self.request("/api/webapp/agents/nova/mcp-presets/docs/connect", "POST", {})["preset"]
        self.assertIn("token=fixture-private-capability", preset["authorizationUrl"])
        self.request("/fixture/consent?token=fixture-private-capability", public=True)
        self.assertEqual(self.request("/api/webapp/agents/nova/mcp-presets/docs/status", "POST", {})["preset"]["status"], "connected")
        with self.assertRaises(HTTPError) as error:
            self.request("/api/webapp/agent-templates", public=True)
        self.assertEqual(error.exception.code, 401)

    def test_terminal_sse_input_resize_close_and_unknown_terminal(self):
        root = "/api/webapp/conversations/api:nova:release/terminal"
        terminal = self.request(root, "POST", {"cols": 80, "rows": 24})["terminalId"]
        path = root + "/" + terminal
        request = Request(self.url + path + "/stream", headers={"Authorization": "Bearer fixture-access"})
        with urlopen(request, timeout=5) as stream:
            self.assertEqual(stream.readline(), b"event: data\n")
            event = json.loads(stream.readline().removeprefix(b"data: "))
            self.assertIn(b"nova@keiki", base64.b64decode(event["data"]))
            stream.readline()
            self.request(path + "/input", "POST", {"data": base64.b64encode(b"pwd\r").decode()})
            self.assertEqual(stream.readline(), b"event: data\n")
            event = json.loads(stream.readline().removeprefix(b"data: "))
            self.assertIn(b"/home/keiki/workspace", base64.b64decode(event["data"]))
            self.assertEqual(event["seq"], 2)
            self.request(path + "/resize", "POST", {"cols": 100, "rows": 30})
            self.request(path, "DELETE")
        with self.assertRaises(HTTPError):
            self.request(root + "/unknown/input", "POST", {"data": ""})


if __name__ == "__main__":
    unittest.main()
