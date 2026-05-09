#!/usr/bin/env python3
"""Black-box regulation tests for the nrb2026 webapp API.

Run against a live server:

    BASE_URL=http://127.0.0.1:8080 python3 webapp/tests/regulation_api_test.py

The test resets application state with POST /api/initialize.
It uses only the Python standard library.
"""

from __future__ import annotations

import base64
import json
import os
import queue
import socket
import threading
import time
import unittest
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

BASE_URL = os.environ.get("BASE_URL", "http://127.0.0.1:8080").rstrip("/")
JPEG_B64 = base64.b64encode(b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01").decode()


class ApiError(AssertionError):
    pass


class JsonWebhookHandler(BaseHTTPRequestHandler):
    received: queue.Queue[dict[str, Any]] = queue.Queue()

    def do_POST(self) -> None:
        length = int(self.headers.get("content-length", "0"))
        body = self.rfile.read(length)
        JsonWebhookHandler.received.put(json.loads(body.decode()))
        self.send_response(204)
        self.end_headers()

    def log_message(self, format: str, *args: Any) -> None:
        return


class WebhookServer:
    def __init__(self) -> None:
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), JsonWebhookHandler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    @property
    def url(self) -> str:
        host, port = self.server.server_address
        return f"http://{host}:{port}/webhook"

    def start(self) -> None:
        self.thread.start()

    def stop(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


def wait_for_port(url: str, timeout_seconds: float = 10.0) -> None:
    host_port = url.removeprefix("http://").removeprefix("https://").split("/", 1)[0]
    host, port_s = host_port.rsplit(":", 1)
    deadline = time.monotonic() + timeout_seconds
    while time.monotonic() < deadline:
        try:
            with socket.create_connection((host, int(port_s)), timeout=1):
                return
        except OSError:
            time.sleep(0.1)
    raise ApiError(f"server did not listen on {host_port}")


def request_json(
    method: str,
    path: str,
    body: dict[str, Any] | None = None,
    user_id: str | None = None,
    expected: int = 200,
    headers: dict[str, str] | None = None,
    timeout: float = 10.0,
) -> tuple[int, dict[str, str], Any]:
    raw_body = None
    req_headers = dict(headers or {})
    if body is not None:
        raw_body = json.dumps(body).encode()
        req_headers["content-type"] = "application/json"
    if user_id is not None:
        req_headers["x-user-id"] = user_id

    req = urllib.request.Request(
        f"{BASE_URL}{path}",
        data=raw_body,
        headers=req_headers,
        method=method,
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as res:
            status = res.status
            response_headers = {k.lower(): v for k, v in res.headers.items()}
            payload = res.read()
    except urllib.error.HTTPError as e:
        status = e.code
        response_headers = {k.lower(): v for k, v in e.headers.items()}
        payload = e.read()
        e.close()

    if status != expected:
        raise ApiError(
            f"{method} {path}: expected {expected}, got {status}, body={payload[:500]!r}"
        )
    if not payload:
        return status, response_headers, None
    return status, response_headers, json.loads(payload.decode())


def request_bytes(
    method: str,
    path: str,
    user_id: str,
    expected: int = 200,
    headers: dict[str, str] | None = None,
) -> tuple[int, dict[str, str], bytes]:
    req_headers = dict(headers or {})
    req_headers["x-user-id"] = user_id
    req = urllib.request.Request(f"{BASE_URL}{path}", headers=req_headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=10) as res:
            status = res.status
            response_headers = {k.lower(): v for k, v in res.headers.items()}
            payload = res.read()
    except urllib.error.HTTPError as e:
        status = e.code
        response_headers = {k.lower(): v for k, v in e.headers.items()}
        payload = e.read()
        e.close()
    if status != expected:
        raise ApiError(
            f"{method} {path}: expected {expected}, got {status}, body={payload[:500]!r}"
        )
    return status, response_headers, payload


def create_user(name: str) -> str:
    _, _, user = request_json("POST", "/api/users", {"name": name})
    assert user["credit_limit"] == 60000
    return user["id"]


def create_campaign(owner_id: str, name: str, price: int = 12000, goal_count: int = 3) -> dict[str, Any]:
    _, _, campaign = request_json(
        "POST",
        "/api/campaigns",
        {
            "name": name,
            "description": f"{name} description",
            "price": price,
            "goal_count": goal_count,
            "tags": ["mesh", "office"],
            "image": JPEG_B64,
        },
        user_id=owner_id,
        expected=201,
    )
    return campaign


class RegulationApiTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        wait_for_port(BASE_URL)
        cls.webhook = WebhookServer()
        cls.webhook.start()
        request_json(
            "POST",
            "/api/initialize",
            {"notification_webhook_url": cls.webhook.url},
            expected=200,
            timeout=120,
        )

    @classmethod
    def tearDownClass(cls) -> None:
        cls.webhook.stop()

    def test_regulation_flow(self) -> None:
        _, _, tags = request_json("GET", "/api/tags")
        self.assertIn("mesh", tags)
        self.assertIn("office", tags)

        request_json("GET", "/api/me", expected=401)
        request_json("POST", "/api/users", {"name": ""}, expected=400)

        owner_id = create_user("reg-owner")
        watcher_id = create_user("reg-watcher")
        closer_id = create_user("reg-closer")
        blocked_id = create_user("reg-blocked")

        _, _, me = request_json("GET", "/api/me", user_id=owner_id)
        self.assertEqual(me["name"], "reg-owner")
        self.assertEqual(me["credit_used"], 0)

        _, _, campaigns = request_json("GET", "/api/campaigns", user_id=owner_id)
        self.assertLessEqual(len(campaigns), 30)
        self.assertTrue(all(c["status"] == "open" for c in campaigns))
        created = [c["created_at"] for c in campaigns]
        self.assertEqual(created, sorted(created, reverse=True))

        request_json("GET", "/api/campaigns?sort=bad", user_id=owner_id, expected=400)
        request_json("GET", "/api/campaigns?tags=mesh,mesh", user_id=owner_id, expected=400)
        request_json("GET", "/api/campaigns?tags=missing-tag", user_id=owner_id, expected=400)
        request_json("GET", "/api/campaigns?tags=mesh,office,gaming,chair", user_id=owner_id, expected=400)
        _, _, mesh_campaigns = request_json(
            "GET",
            "/api/campaigns?tags=mesh&sort=active",
            user_id=owner_id,
        )
        self.assertLessEqual(len(mesh_campaigns), 30)
        self.assertTrue(all("mesh" in c["tags"] for c in mesh_campaigns))

        request_json(
            "POST",
            "/api/campaigns",
            {
                "name": "invalid price",
                "description": "invalid price",
                "price": 1999,
                "goal_count": 3,
                "tags": ["mesh"],
                "image": JPEG_B64,
            },
            user_id=owner_id,
            expected=400,
        )

        campaign = create_campaign(owner_id, "reg-campaign-main", price=12000, goal_count=3)
        campaign_id = campaign["id"]
        self.assertEqual(campaign["current_count"], 0)
        self.assertEqual(campaign["status"], "open")
        self.assertEqual(campaign["participants"], [])
        _, _, mesh_office_campaigns = request_json(
            "GET",
            "/api/campaigns?tags=mesh,office",
            user_id=owner_id,
        )
        self.assertTrue(any(c["id"] == campaign_id for c in mesh_office_campaigns))

        _, _, detail = request_json("GET", f"/api/campaigns/{campaign_id}", user_id=owner_id)
        self.assertEqual(detail["id"], campaign_id)
        _, image_headers, image = request_bytes(
            "GET",
            f"/api/campaigns/{campaign_id}/image",
            user_id=owner_id,
        )
        self.assertTrue(image.startswith(b"\xff\xd8\xff"))
        self.assertEqual(image_headers["content-type"], "image/jpeg")
        etag = image_headers["etag"]
        request_bytes(
            "GET",
            f"/api/campaigns/{campaign_id}/image",
            user_id=owner_id,
            expected=304,
            headers={"if-none-match": etag},
        )

        request_json(
            "POST",
            "/api/saved_searches",
            {"tags": ["mesh"]},
            user_id=watcher_id,
            expected=201,
        )
        request_json(
            "POST",
            "/api/saved_searches",
            {"tags": ["mesh", "mesh"]},
            user_id=watcher_id,
            expected=400,
        )

        _, _, joined = request_json(
            "POST",
            f"/api/campaigns/{campaign_id}/join",
            {},
            user_id=owner_id,
        )
        self.assertEqual(joined["current_count"], 1)
        request_json(
            "POST",
            f"/api/campaigns/{campaign_id}/join",
            {},
            user_id=owner_id,
            expected=409,
        )
        _, _, me = request_json("GET", "/api/me", user_id=owner_id)
        self.assertEqual(me["credit_used"], 12000)

        _, _, joined = request_json(
            "POST",
            f"/api/campaigns/{campaign_id}/join",
            {},
            user_id=watcher_id,
        )
        self.assertEqual(joined["current_count"], 2)
        webhook_payload = JsonWebhookHandler.received.get(timeout=5)
        self.assertEqual(webhook_payload["type"], "campaign_closing_soon")
        self.assertEqual(webhook_payload["user_id"], watcher_id)
        self.assertEqual(webhook_payload["campaign"]["id"], campaign_id)

        _, _, joined = request_json(
            "POST",
            f"/api/campaigns/{campaign_id}/join",
            {},
            user_id=closer_id,
        )
        self.assertEqual(joined["current_count"], 3)
        self.assertEqual(joined["status"], "closed")
        request_json(
            "POST",
            f"/api/campaigns/{campaign_id}/join",
            {},
            user_id=blocked_id,
            expected=409,
        )

        for user_id in (owner_id, watcher_id, closer_id):
            _, _, charges = request_json("GET", "/api/charges", user_id=user_id)
            self.assertEqual(charges[0]["amount"], 12000)
            self.assertEqual(charges[0]["campaign"]["id"], campaign_id)
            _, _, me = request_json("GET", "/api/me", user_id=user_id)
            self.assertEqual(me["credit_used"], 0)

        for i in range(10):
            request_json(
                "POST",
                "/api/saved_searches",
                {"tags": ["office"]},
                user_id=blocked_id,
                expected=201,
            )
        request_json(
            "POST",
            "/api/saved_searches",
            {"tags": ["mesh"]},
            user_id=blocked_id,
            expected=409,
        )

        credit_user_id = create_user("reg-credit-limit")
        credit_campaigns = [
            create_campaign(owner_id, f"reg-credit-{i}", price=20000, goal_count=20)
            for i in range(4)
        ]
        for campaign in credit_campaigns[:3]:
            request_json(
                "POST",
                f"/api/campaigns/{campaign['id']}/join",
                {},
                user_id=credit_user_id,
            )
        _, _, me = request_json("GET", "/api/me", user_id=credit_user_id)
        self.assertEqual(me["credit_used"], 60000)
        request_json(
            "POST",
            f"/api/campaigns/{credit_campaigns[3]['id']}/join",
            {},
            user_id=credit_user_id,
            expected=402,
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
