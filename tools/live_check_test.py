"""Offline tests for tools/live-check.py. Every fixture is synthetic."""

import base64
import contextlib
import importlib.util
import io
import json
import os
import signal
import stat
import subprocess
import sys
import tempfile
import unittest
from collections import Counter
from datetime import datetime, timedelta, timezone
from pathlib import Path

_SPEC = importlib.util.spec_from_file_location("live_check", Path(__file__).with_name("live-check.py"))
lc = importlib.util.module_from_spec(_SPEC)
sys.modules[_SPEC.name] = lc
_SPEC.loader.exec_module(lc)

NOW = datetime(2030, 1, 1, tzinfo=timezone.utc)


def jwt(claims):
    part = lambda obj: base64.urlsafe_b64encode(json.dumps(obj).encode()).rstrip(b"=").decode()
    return f"{part({'alg': 'none'})}.{part(claims)}.c2ln"


def rfc3339(moment):
    return moment.strftime("%Y-%m-%dT%H:%M:%SZ")


class TempDir(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self._tmp.name)

    def tearDown(self):
        self._tmp.cleanup()

    def write(self, name, data, directory=None):
        path = (directory or self.dir) / name
        path.write_text(json.dumps(data))
        return path


class SelectionTest(TempDir):
    def selected(self, provider):
        return sorted(p.name for p in lc.select(self.dir, provider, NOW).files)

    def test_type_is_trimmed_and_lower_cased(self):
        self.write("a.json", {"type": " Claude "})
        self.write("b.json", {"type": "KIMI.COM"})
        self.assertEqual(self.selected("claude"), ["a.json"])
        self.assertEqual(self.selected("kimi"), ["b.json"])

    def test_disabled_only_as_boolean_true(self):
        self.write("string.json", {"type": "codex", "disabled": "true"})
        self.write("bool.json", {"type": "codex", "disabled": True})
        selection = lc.select(self.dir, "codex", NOW)
        self.assertEqual([p.name for p in selection.files], ["string.json"])
        self.assertEqual(selection.skipped, Counter({"disabled": 1}))

    def test_claude_expiry_reads_only_rfc3339_expired(self):
        past, future = rfc3339(NOW - timedelta(hours=1)), rfc3339(NOW + timedelta(hours=1))
        expired_jwt = jwt({"exp": int((NOW - timedelta(hours=1)).timestamp())})
        self.write("jwt.json", {"type": "claude", "access_token": expired_jwt, "expired": future})
        self.write("other-key.json", {"type": "claude", "expires_at": past})
        self.write("not-rfc3339.json", {"type": "claude", "expired": "2000-01-01 00:00:00"})
        self.write("past.json", {"type": "claude", "expired": past})
        selection = lc.select(self.dir, "claude", NOW)
        self.assertEqual(sorted(p.name for p in selection.files), ["jwt.json", "not-rfc3339.json", "other-key.json"])
        self.assertEqual(selection.skipped, Counter({"token expired": 1}))

    def test_other_providers_prefer_jwt_exp_then_metadata_keys(self):
        past = NOW - timedelta(hours=1)
        future = NOW + timedelta(hours=1)
        self.write("jwt-wins.json", {"type": "codex", "access_token": jwt({"exp": int(future.timestamp())}), "expired": rfc3339(past)})
        self.write("jwt-past.json", {"type": "codex", "access_token": jwt({"exp": int(past.timestamp())}), "expired": rfc3339(future)})
        self.write("expire.json", {"type": "codex", "access_token": "opaque", "expire": rfc3339(past)})
        self.write("millis.json", {"type": "codex", "expires_at": int(past.timestamp() * 1000)})
        self.write("numeric-string.json", {"type": "codex", "expiry": str(int(past.timestamp()))})
        self.write("naive.json", {"type": "codex", "expires": past.strftime("%Y-%m-%d %H:%M")})
        self.write("order.json", {"type": "codex", "expired": rfc3339(future), "expires": rfc3339(past)})
        selection = lc.select(self.dir, "codex", NOW)
        self.assertEqual(sorted(p.name for p in selection.files), ["jwt-wins.json", "order.json"])
        self.assertEqual(selection.skipped, Counter({"token expired": 5}))

    def test_a_file_without_a_readable_expiry_is_kept(self):
        self.write("none.json", {"type": "xai"})
        self.write("garbage.json", {"type": "xai", "access_token": "a.b.c", "expired": "soon"})
        self.assertEqual(self.selected("xai"), ["garbage.json", "none.json"])

    def test_expiring_within_ten_minutes_is_skipped(self):
        self.write("nine.json", {"type": "kimi", "expired": rfc3339(NOW + timedelta(minutes=9))})
        self.write("eleven.json", {"type": "kimi", "expired": rfc3339(NOW + timedelta(minutes=11))})
        self.assertEqual(self.selected("kimi"), ["eleven.json"])

    def test_other_types_are_counted_once_as_not_covered(self):
        self.write("a.json", {"type": "gemini"})
        self.write("b.json", {"type": "antigravity"})
        self.write("c.json", {"type": "claude"})
        self.write("d.json", {})
        (self.dir / "e.txt").write_text("{}")
        self.assertEqual(lc.count_not_covered(self.dir), 2)
        self.assertEqual(self.selected("claude"), ["c.json"])


class StripTest(TempDir):
    def test_strip_removes_both_spellings_at_any_depth_and_keeps_the_rest(self):
        data = {
            "type": "codex",
            "refresh_token": "r1",
            "refreshToken": "r2",
            "access_token": "a",
            "n": 3,
            "token": {"refresh_token": "r3", "id_token": "i", "list": [{"refreshToken": "r4", "x": None}]},
        }
        expected = {"type": "codex", "access_token": "a", "n": 3, "token": {"id_token": "i", "list": [{"x": None}]}}
        self.assertEqual(lc.strip(data), expected)

    def test_strip_removes_other_cases_of_the_key(self):
        self.assertEqual(lc.strip({"REFRESH_TOKEN": "r", "Refresh_Token": "r", "keep": 1}), {"keep": 1})

    def test_copies_are_private_and_stripped(self):
        src = self.write("a.json", {"type": "claude", "refresh_token": "synthetic-refresh-value", "email": "e"})
        dest = self.dir / "copy"
        lc.copy_stripped([src], dest)
        self.assertEqual(stat.S_IMODE(dest.stat().st_mode), 0o700)
        copy = dest / "a.json"
        self.assertEqual(stat.S_IMODE(copy.stat().st_mode), 0o600)
        self.assertEqual(json.loads(copy.read_text()), {"type": "claude", "email": "e"})
        lc.verify_copies([src], dest)


class VerifyTest(TempDir):
    def setUp(self):
        super().setUp()
        self.dest = self.dir / "copy"
        self.dest.mkdir()

    def pair(self, original, copy_text):
        src = self.write("a.json", original)
        (self.dest / "a.json").write_text(copy_text)
        return src

    def test_verify_rejects_a_remaining_key(self):
        src = self.pair({"refresh_token": "synthetic-1"}, json.dumps({"nested": {"refreshToken": ""}}))
        with self.assertRaises(lc.SetupError):
            lc.verify_copies([src], self.dest)

    def test_verify_rejects_a_remaining_value(self):
        src = self.pair({"refresh_token": "synthetic-refresh-2"}, json.dumps({"note": "synthetic-refresh-2"}))
        with self.assertRaises(lc.SetupError):
            lc.verify_copies([src], self.dest)

    def test_verify_rejects_an_escaped_value(self):
        src = self.pair({"refresh_token": "synthetic/refresh/3"}, '{"note": "synthetic\\/refresh\\/3"}')
        with self.assertRaises(lc.SetupError):
            lc.verify_copies([src], self.dest)

    def test_verify_rejects_a_value_written_as_a_number(self):
        src = self.pair({"refresh_token": "123456789"}, json.dumps({"n": 123456789}))
        with self.assertRaises(lc.SetupError):
            lc.verify_copies([src], self.dest)

    def test_verify_rejects_a_missing_copy(self):
        src = self.write("missing.json", {"refresh_token": "synthetic-4"})
        with self.assertRaises(lc.SetupError):
            lc.verify_copies([src], self.dest)

    def test_verify_accepts_a_clean_copy(self):
        src = self.pair({"refresh_token": "synthetic-5", "access_token": "a"}, json.dumps({"access_token": "a"}))
        lc.verify_copies([src], self.dest)


class ChildTest(unittest.TestCase):
    def test_child_environment_holds_only_the_allowlist(self):
        caller = {
            "PATH": "/bin",
            "HOME": "/home/synthetic",
            "HTTPS_PROXY": "http://proxy.invalid:1",
            "no_proxy": "localhost",
            "HOME_JWT": "x",
            "home_jwt": "x",
            "PGSTORE_DSN": "x",
            "pgstore_dsn": "x",
            "DEPLOY": "cloud",
            "MANAGEMENT_PASSWORD": "x",
            "OBJECTSTORE_ENDPOINT": "x",
            "WRITABLE_PATH": "/elsewhere",
        }
        env = lc.child_env(caller, Path("/run/dir"))
        self.assertEqual(
            env,
            {
                "PATH": "/bin",
                "HOME": "/home/synthetic",
                "HTTPS_PROXY": "http://proxy.invalid:1",
                "no_proxy": "localhost",
                "WRITABLE_PATH": "/run/dir",
            },
        )

    def test_config_disables_retry_and_has_no_management_or_native_block(self):
        doc = json.loads(lc.config_text(41234, "client-key", Path("/run/auth")))
        self.assertEqual(doc["server"], {"host": "127.0.0.1", "port": 41234})
        self.assertEqual(doc["access"]["api-keys"], ["client-key"])
        self.assertEqual(doc["oauth"]["auth-dir"], "/run/auth")
        self.assertEqual(doc["routing"]["retry"], {"request-retry": 0, "max-retry-credentials": 1})
        self.assertFalse(doc["observability"]["logs"]["request-log"])
        self.assertFalse(doc["observability"]["history"]["enable"])
        self.assertNotIn("management", doc)
        self.assertNotIn("secret-key", json.dumps(doc))
        self.assertNotIn("native", json.dumps(doc))


class LeftoverTest(TempDir):
    def run_dir(self, name, pid):
        path = self.dir / f"cliproxy-live-{name}"
        path.mkdir()
        if pid is not None:
            (path / "pid").write_text(str(pid))
        (path / "auth").mkdir()
        (path / "auth" / "a.json").write_text("{}")
        return path

    def test_cleanup_deletes_a_dead_run_and_keeps_a_live_one(self):
        child = subprocess.Popen([sys.executable, "-c", "pass"])
        child.wait()
        dead = self.run_dir("dead", child.pid)
        live = self.run_dir("live", os.getpid())
        other = self.dir / "unrelated"
        other.mkdir()
        (other / "pid").write_text(str(child.pid))
        lc.clean_leftovers(self.dir)
        self.assertFalse(dead.exists())
        self.assertTrue(live.exists())
        self.assertTrue(other.exists())

    def test_cleanup_keeps_a_directory_without_a_pid(self):
        pending = self.run_dir("pending", None)
        lc.clean_leftovers(self.dir)
        self.assertTrue(pending.exists())

    def test_run_dir_is_private_and_records_the_pid(self):
        run = lc.make_run_dir(self.dir)
        self.assertTrue(run.name.startswith("cliproxy-live-"))
        self.assertEqual(stat.S_IMODE(run.stat().st_mode), 0o700)
        pid = run / "pid"
        self.assertEqual(pid.read_text() if pid.exists() else None, str(os.getpid()))


def sse(*events):
    out = []
    for name, data in events:
        if name:
            out.append(f"event: {name}")
        out.append("data: " + (data if isinstance(data, str) else json.dumps(data)))
        out.append("")
    return ("\n".join(out) + "\n").encode()


MESSAGES_STREAM = sse(
    ("message_start", {"type": "message_start"}),
    ("content_block_delta", {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "ok"}}),
    ("message_stop", {"type": "message_stop"}),
)
CHAT_STREAM = sse((None, {"choices": [{"delta": {"content": "ok"}}]}), (None, "[DONE]"))
RESPONSES_STREAM = sse(
    ("response.output_text.delta", {"type": "response.output_text.delta", "delta": "ok"}),
    ("response.completed", {"type": "response.completed", "response": {"output": []}}),
)


class ReplyTest(unittest.TestCase):
    def ok(self, route, stream, body):
        self.assertIsNone(lc.check_reply(route, stream, 200, body))

    def bad(self, route, stream, body, status=200):
        reason = lc.check_reply(route, stream, status, body)
        self.assertIsNotNone(reason)
        return reason

    def test_messages(self):
        self.ok("/v1/messages", False, json.dumps({"content": [{"type": "text", "text": "ok"}]}).encode())
        self.bad("/v1/messages", False, json.dumps({"content": [{"type": "text", "text": "  "}]}).encode())
        self.ok("/v1/messages", True, MESSAGES_STREAM)
        self.bad("/v1/messages", True, MESSAGES_STREAM.replace(b"message_stop", b"message_delta"))
        self.bad("/v1/messages", True, MESSAGES_STREAM.replace(b'"text": "ok"', b'"text": ""'))

    def test_chat_completions(self):
        self.ok("/v1/chat/completions", False, json.dumps({"choices": [{"message": {"content": "ok"}}]}).encode())
        self.bad("/v1/chat/completions", False, json.dumps({"choices": [{"message": {"content": None}}]}).encode())
        self.ok("/v1/chat/completions", True, CHAT_STREAM)
        self.bad("/v1/chat/completions", True, CHAT_STREAM.replace(b"[DONE]", b"{}"))

    def test_responses(self):
        body = {"output": [{"type": "reasoning"}, {"type": "message", "content": [{"type": "output_text", "text": "ok"}]}]}
        self.ok("/v1/responses", False, json.dumps(body).encode())
        self.bad("/v1/responses", False, json.dumps({"output": [{"type": "reasoning"}]}).encode())
        self.ok("/v1/responses", True, RESPONSES_STREAM)
        self.bad("/v1/responses", True, RESPONSES_STREAM.replace(b"response.completed", b"response.in_progress"))
        completed_only = sse(("response.completed", {"type": "response.completed", "response": body}))
        self.ok("/v1/responses", True, completed_only)

    def test_a_non_200_status_fails_with_the_start_of_the_body(self):
        reason = self.bad("/v1/messages", False, b"x" * 300, status=400)
        self.assertEqual(reason, "400 " + "x" * 200)

    def test_pick_model_skips_media_models(self):
        models = {"data": [{"id": "grok-imagine-image"}, {"id": "grok-4.7"}, {"id": "grok-4"}]}
        self.assertEqual(lc.pick_model("xai", json.dumps(models).encode()), "grok-4")
        self.assertIsNone(lc.pick_model("xai", json.dumps({"data": [{"id": "grok-imagine-video"}]}).encode()))


class ReportTest(TempDir):
    def setUp(self):
        super().setUp()
        self.name = "claude-synthetic-account-7.json"
        self.src = self.write(self.name, {"type": "claude", "email": "synthetic.user@example.test", "access_token": "sk-synthetic-access"})
        self.redactor = lc.Redactor([self.src])

    def outcomes(self, failure=None):
        out = [lc.Outcome(route, stream, 200, None) for route, stream in lc.ROUTES]
        if failure:
            out[-1] = failure
        return out

    def test_a_passing_provider(self):
        result = lc.ProviderResult("claude", 2, Counter({"token expired": 1}), self.outcomes())
        self.assertEqual(lc.report_line(result, self.redactor), "claude: pass (6/6), 2 accounts, skipped (1 token expired)")

    def test_a_skipped_provider(self):
        result = lc.ProviderResult("xai", 0, Counter({"token expired": 2, "disabled": 1}), [])
        self.assertEqual(lc.report_line(result, self.redactor), "xai: skipped (1 disabled, 2 token expired)")
        self.assertEqual(lc.report_line(lc.ProviderResult("kimi", 0, Counter(), []), self.redactor), "kimi: skipped (no credentials)")

    def test_a_401_line_mentions_the_installed_service(self):
        failure = lc.Outcome("/v1/responses", True, 401, "401 unauthorized")
        line = lc.report_line(lc.ProviderResult("claude", 1, Counter(), self.outcomes(failure)), self.redactor)
        self.assertTrue(line.startswith("claude: FAIL (5/6) /v1/responses stream: 401 unauthorized"), line)
        self.assertIn("installed service may have refreshed", line)
        self.assertIn("rerun", line)

    def test_report_lines_never_name_a_source_file(self):
        body = f"400 bad credential {self.name} for synthetic.user@example.test token sk-synthetic-access"
        failure = lc.Outcome("/v1/responses", True, 400, body)
        lines = [
            lc.report_line(lc.ProviderResult("claude", 1, Counter(), self.outcomes(failure)), self.redactor),
            lc.report_line(lc.ProviderResult("claude", 1, Counter(), [], error=body), self.redactor),
            lc.report_line(lc.ProviderResult("claude", 0, Counter({"disabled": 1}), []), self.redactor),
        ]
        for line in lines:
            for secret in (self.name, Path(self.name).stem, "synthetic.user@example.test", "sk-synthetic-access"):
                self.assertNotIn(secret, line)
        self.assertIn("bad credential", lines[0])


FAKE_SERVER = r"""#!/usr/bin/env python3
import json, os, sys
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

args = dict(zip(sys.argv[1::2], sys.argv[2::2]))
config = json.loads(Path(args["--config"]).read_text())
auth = Path(config["oauth"]["auth-dir"])
key = config["access"]["api-keys"][0]
mode = Path(sys.argv[0]).with_name("mode").read_text().strip()
healthy = (
    "HOME_JWT" not in os.environ
    and os.environ.get("WRITABLE_PATH")
    and all("refresh" not in p.read_text().lower() for p in auth.glob("*.json"))
)

def sse(events):
    return "".join(("event: %s\n" % n if n else "") + "data: " + (d if isinstance(d, str) else json.dumps(d)) + "\n\n" for n, d in events).encode()

REPLIES = {
    ("/v1/messages", False): json.dumps({"content": [{"type": "text", "text": "ok"}]}).encode(),
    ("/v1/messages", True): sse([("content_block_delta", {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "ok"}}), ("message_stop", {"type": "message_stop"})]),
    ("/v1/chat/completions", False): json.dumps({"choices": [{"message": {"content": "ok"}}]}).encode(),
    ("/v1/chat/completions", True): sse([(None, {"choices": [{"delta": {"content": "ok"}}]}), (None, "[DONE]")]),
    ("/v1/responses", False): json.dumps({"output": [{"type": "message", "content": [{"type": "output_text", "text": "ok"}]}]}).encode(),
    ("/v1/responses", True): sse([("response.output_text.delta", {"type": "response.output_text.delta", "delta": "ok"}), ("response.completed", {"type": "response.completed", "response": {}})]),
}

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def send(self, status, body):
        self.send_response(status)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/healthz":
            return self.send(200 if healthy else 500, b"")
        if self.headers.get("Authorization") != "Bearer " + key:
            return self.send(401, b"no key")
        ids = ["claude-haiku-synthetic", "gpt-synthetic-mini", "grok-synthetic", "kimi-k2-synthetic"]
        if self.headers.get("anthropic-version"):
            # An Anthropic client gets the list in Claude form, other providers' ids disguised.
            ids = ["claude-haiku-synthetic", "claude-disguised-synthetic"]
        self.send(200, json.dumps({"data": [{"id": i} for i in ids]}).encode())

    def do_POST(self):
        if self.headers.get("Authorization") != "Bearer " + key:
            return self.send(401, b"no key")
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        route = (self.path, bool(body.get("stream")))
        if mode == "fail" and route == ("/v1/responses", True):
            return self.send(400, b"synthetic rejection")
        self.send(200, REPLIES[route])

HTTPServer(("127.0.0.1", config["server"]["port"]), Handler).serve_forever()
"""


class RunTest(TempDir):
    def setUp(self):
        super().setUp()
        self.auth = self.dir / "auth"
        self.auth.mkdir()
        self.tmp = self.dir / "tmp"
        self.tmp.mkdir()
        fake = self.dir / "bin" / "cliproxy"
        fake.parent.mkdir()
        fake.write_text(FAKE_SERVER)
        fake.chmod(0o755)
        self.mode = fake.with_name("mode")
        self.mode.write_text("pass")
        saved = (lc._build, tempfile.tempdir, os.environ.get("HOME_JWT"), signal.getsignal(signal.SIGINT), signal.getsignal(signal.SIGTERM))

        def restore():
            lc._build, tempfile.tempdir = saved[0], saved[1]
            if saved[2] is None:
                os.environ.pop("HOME_JWT", None)
            else:
                os.environ["HOME_JWT"] = saved[2]
            signal.signal(signal.SIGINT, saved[3])
            signal.signal(signal.SIGTERM, saved[4])

        self.addCleanup(restore)
        lc._build = lambda: fake
        tempfile.tempdir = str(self.tmp)
        os.environ["HOME_JWT"] = "synthetic"

    def run_tool(self, *args):
        out = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(out):
            code = lc.run(["--auth-dir", str(self.auth), *args])
        return code, out.getvalue()

    def test_a_run_passes_with_stripped_copies_and_leaves_nothing_behind(self):
        self.write("a.json", {"type": "claude", "refresh_token": "synthetic-refresh", "access_token": "synthetic-access"}, self.auth)
        self.write("b.json", {"type": "gemini"}, self.auth)
        before = {p: p.read_bytes() for p in self.auth.iterdir()}
        code, out = self.run_tool()
        self.assertEqual(code, 0, out)
        self.assertIn("claude: pass (6/6), 1 account", out)
        self.assertIn("codex: skipped (no credentials)", out)
        self.assertIn("not covered: 1 credential of other types", out)
        self.assertEqual(list(self.tmp.iterdir()), [])
        self.assertEqual({p: p.read_bytes() for p in self.auth.iterdir()}, before)

    def test_a_rejected_request_fails_the_run(self):
        self.mode.write_text("fail")
        self.write("a.json", {"type": "codex"}, self.auth)
        code, out = self.run_tool("--provider", "codex")
        self.assertEqual(code, 1, out)
        self.assertIn("codex: FAIL (5/6) /v1/responses stream: 400 synthetic rejection", out)
        self.assertEqual(list(self.tmp.iterdir()), [])

    def test_a_run_with_only_skipped_credentials_checks_nothing(self):
        self.write("a.json", {"type": "xai", "disabled": True}, self.auth)
        code, out = self.run_tool()
        self.assertEqual(code, 3, out)
        self.assertIn("xai: skipped (1 disabled)", out)


if __name__ == "__main__":
    unittest.main()
