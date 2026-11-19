#!/usr/bin/env python3
"""Loopback-only Keiki/OAuth fixture for the real zgx engine and headed UI.

Run with an isolated HOME and Secret Service for the engine, never a live account.
Only fake credentials are accepted. Requests are logged without headers/bodies.
"""
import argparse
import base64
import hashlib
import json
import threading
import time
import uuid
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, unquote, urlencode, urlparse


def message(identity, text, direction):
    return {"id": identity, "content": text, "direction": direction,
            "createdAt": datetime.now(timezone.utc).isoformat(), "internal": False}


class Fixture:
    def __init__(self):
        self.org = "studio"
        self.clients = {}
        self.codes = {}
        self.lock = threading.Lock()
        self.agents = [{"id": "nova", "name": "Nova", "model": "copilot", "active": True},
                       {"id": "fern", "name": "Fern", "model": "copilot", "active": True}]
        self.messages = {"api:nova:release": [
            message("import-1", "Can we make the launch feel a little more considered?", "inbound"),
            message("import-2", "Absolutely. Let’s start with a clear story, a focused checklist, and room for a little delight.", "outbound")],
            "api:nova:design": [message("design-1", "Let’s explore the visual direction together.", "outbound")]}
        self.live = set()
        self.replies = 0

    def conversations(self, agent):
        return [{"phone": phone, "contactName": "Launch notes" if phone.endswith("release") else "Design explorations",
                 "agentId": agent, "agentName": "Nova", "lastMessage": rows[-1]["content"] if rows else "",
                 "messageCount": len(rows), "lastMessageAt": rows[-1]["createdAt"] if rows else "",
                 "lastDirection": "outbound", "isActive": phone in self.live, "hasErrors": False}
                for phone, rows in self.messages.items() if phone.startswith(f"api:{agent}:")]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, _format, *_args):
        pass

    @property
    def fixture(self):
        return self.server.fixture

    def reply(self, body, status=200):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        self.dispatch()

    def do_POST(self):
        self.dispatch()

    def do_DELETE(self):
        self.dispatch()

    def dispatch(self):
        parsed = urlparse(self.path)
        path, query = unquote(parsed.path), parse_qs(parsed.query)
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length).decode() if length else ""
        body = json.loads(raw) if raw and "application/json" in self.headers.get("Content-Type", "") else parse_qs(raw)
        print(f"{self.command} {path}", flush=True)
        base = f"http://127.0.0.1:{self.server.server_port}"
        if path == "/.well-known/oauth-authorization-server":
            return self.reply({"issuer": base, "authorization_endpoint": base + "/oauth/authorize",
                               "token_endpoint": base + "/oauth/token", "registration_endpoint": base + "/oauth/register",
                               "revocation_endpoint": base + "/oauth/revoke", "code_challenge_methods_supported": ["S256"],
                               "scopes_supported": ["manage", "offline_access"]})
        if path == "/.well-known/oauth-protected-resource/api/webapp":
            return self.reply({"resource": base + "/api/webapp", "authorization_servers": [base], "scopes_supported": ["manage"], "bearer_methods_supported": ["header"]})
        if path == "/oauth/register":
            client = "fixture-client-" + uuid.uuid4().hex
            self.fixture.clients[client] = body["redirect_uris"]
            return self.reply({"client_id": client, "redirect_uris": body["redirect_uris"], "token_endpoint_auth_method": "none"})
        if path == "/oauth/authorize":
            client, redirect = query["client_id"][0], query["redirect_uri"][0]
            if redirect not in self.fixture.clients.get(client, []) or query.get("code_challenge_method") != ["S256"]:
                return self.reply({"error": "invalid_request"}, 400)
            code = uuid.uuid4().hex
            self.fixture.codes[code] = (client, redirect, query["code_challenge"][0])
            target = redirect + "?" + urlencode({"code": code, "state": query["state"][0]})
            self.send_response(302)
            self.send_header("Location", target)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if path == "/oauth/token":
            if body.get("grant_type") == ["authorization_code"]:
                expected = self.fixture.codes.pop(body.get("code", [""])[0], None)
                challenge = base64.urlsafe_b64encode(hashlib.sha256(body.get("code_verifier", [""])[0].encode()).digest()).rstrip(b"=").decode()
                if expected != (body.get("client_id", [""])[0], body.get("redirect_uri", [""])[0], challenge):
                    return self.reply({"error": "invalid_grant"}, 400)
                print("OAuth PKCE exchange verified", flush=True)
            elif body.get("refresh_token") != ["fixture-refresh"]:
                return self.reply({"error": "invalid_grant"}, 400)
            return self.reply({"access_token": "fixture-access", "refresh_token": "fixture-refresh", "token_type": "Bearer",
                               "expires_in": 3600, "scope": "manage offline_access"})
        if path == "/oauth/revoke":
            return self.reply({})
        if self.headers.get("Authorization") != "Bearer fixture-access":
            return self.reply({"error": "unauthorized"}, 401)
        if path == "/api/webapp/auth/session":
            return self.reply({"user": {"name": "Keiki Demo", "email": "demo@keiki.test", "role": "member",
                                       "orgId": self.fixture.org, "activeOrgId": self.fixture.org,
                                       "orgs": [{"id": "studio", "name": "Keiki Studio"}, {"id": "research", "name": "Research"}]}})
        if path == "/api/webapp/orgs/switch":
            if body.get("orgId") not in ("studio", "research"):
                return self.reply({"error": "invalid_org"}, 400)
            self.fixture.org = body["orgId"]
            return self.reply({"ok": True, "activeOrgId": self.fixture.org})
        if path == "/api/webapp/agents":
            return self.reply({"agents": self.fixture.agents if self.fixture.org == "studio" else []})
        if path == "/api/webapp/agent-groups":
            return self.reply({"groups": [{"id": "design", "name": "Design studio", "members": [{"agentId": "nova", "name": "Nova"}]}] if self.fixture.org == "studio" else []})
        if path == "/api/webapp/agent-groups/design/roster":
            return self.reply({"members": [{"agentId": "nova", "name": "Nova", "status": "running" if self.fixture.live else "idle"}], "totals": {"members": 1}})
        if path == "/api/webapp/conversations" or path.startswith("/api/webapp/agents/"):
            agent = path.split("/")[4] if path.startswith("/api/webapp/agents/") else "nova"
            offset, limit = int(query.get("offset", [0])[0]), int(query.get("limit", [50])[0])
            return self.reply({"conversations": self.fixture.conversations(agent)[offset:offset + limit]})
        if path.startswith("/api/webapp/conversations/"):
            identity = path.removeprefix("/api/webapp/conversations/")
            if identity.endswith("/steer"):
                return self.steer(identity.removesuffix("/steer"), body)
            if identity.endswith("/takeover"):
                return self.reply({"takeover": None})
            rows = self.fixture.messages.get(identity, [])
            offset, limit = int(query.get("offset", [0])[0]), int(query.get("limit", [50])[0])
            return self.reply({"phone": identity, "meta": {"agentId": query.get("agentId", ["nova"])[0], "messageCount": len(rows)},
                               "messages": rows[offset:offset + limit], "asks": [], "blocked": False, "takeover": None,
                               "activeTurnStartedAt": "now" if identity in self.fixture.live else None, "liveness": None})
        return self.reply({"error": "not_found"}, 404)

    def steer(self, identity, body):
        text = body.get("text", "")
        rows = self.fixture.messages.setdefault(identity, [])
        rows.append(message(uuid.uuid4().hex, text, "inbound"))
        self.fixture.live.add(identity)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()
        run, answer = uuid.uuid4().hex, uuid.uuid4().hex
        with self.fixture.lock:
            self.fixture.replies += 1
            number = self.fixture.replies
        reply = f"Launch update {number}: for “{text}”, Nova suggests one owner, one useful example, and a clear next step for the team."

        def event(kind, **fields):
            self.wfile.write(("data: " + json.dumps({"type": kind, **fields}) + "\n\n").encode())
            self.wfile.flush()

        try:
            event("RUN_STARTED", threadId=identity, runId=run)
            event("TEXT_MESSAGE_START", messageId=answer, role="assistant")
            for word in reply.split(" "):
                event("TEXT_MESSAGE_CONTENT", messageId=answer, delta=word + " ")
                time.sleep(0.35)
            event("TEXT_MESSAGE_END", messageId=answer)
            rows.append(message(answer, reply, "outbound"))
            event("RUN_FINISHED", threadId=identity, runId=run,
                  outcome={"type": "interrupt", "interrupts": [{"id": "launch-direction", "reason": "Choose a direction",
                           "message": "Should we lead with the story or the product?"}]})
        except (BrokenPipeError, ConnectionResetError):
            print("Stream disconnected (local Stop)", flush=True)
        finally:
            self.fixture.live.discard(identity)
            self.close_connection = True


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=8765)
    options = parser.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", options.port), Handler)
    server.fixture = Fixture()
    print(f"Fake Keiki listening on http://127.0.0.1:{server.server_port}", flush=True)
    server.serve_forever()
