#!/usr/bin/env python3
"""Build the working tree and send a few real requests through its translating proxy.

Each signed-in account file is copied into a private temp directory with every refresh
token removed, and a dev server runs against that copy, one provider at a time. A
credential without a refresh token is never refreshed, so a broken working tree cannot
rotate a token and sign the installed service's account out. The source directory is
only read.

Exit codes: 0 every checked provider passed, 1 a provider failed, 2 setup failed,
3 nothing was checked because every credential was skipped.
"""

from __future__ import annotations

import argparse
import base64
import http.client
import json
import math
import os
import re
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from collections import Counter
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import List, Optional

REPO = Path(__file__).resolve().parent.parent
RUN_PREFIX = "cliproxy-live-"
EXPIRY_MARGIN = timedelta(minutes=10)
HEALTH_TIMEOUT = 30.0
REQUEST_TIMEOUT = 180.0
EXCERPT = 200
# A reasoning model spends part of the cap before it writes text.
OUTPUT_CAP = 1024
PROMPT = "Reply with the single word ok."

# Provider name to the auth-file `type` values it covers.
PROVIDERS = {
    "claude": ("claude",),
    "codex": ("codex",),
    "xai": ("xai",),
    "kimi": ("kimi", "kimi-ai", "kimi.ai", "kimi.com"),
}
# Model id substrings in order of preference; the shortest match of the first hit wins.
MODEL_PREFERENCE = {
    "claude": ("claude-haiku", "claude-sonnet", "claude-"),
    # The Codex client's first-priority model; a ChatGPT account refuses some listed ids.
    "codex": ("gpt-6.1-sol", "-sol", "gpt-"),
    "xai": ("non-reasoning", "grok-3-mini", "grok-"),
    "kimi": ("kimi-for-coding", "kimi-k2", "kimi-"),
}
MODEL_EXCLUDE = ("imagine", "image", "video", "embed", "tts", "audio", "realtime", "transcri", "multi-agent")
ROUTES = [
    ("/v1/messages", False),
    ("/v1/messages", True),
    ("/v1/chat/completions", False),
    ("/v1/chat/completions", True),
    ("/v1/responses", False),
    ("/v1/responses", True),
]
CHILD_ENV_ALLOW = (
    "PATH",
    "HOME",
    "TMPDIR",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
)
# Keys whose string values are not identifying and stay readable in error excerpts.
REDACT_SKIP_KEYS = ("type",)
# Shorter values identify nobody, and replacing them would garble the excerpt.
REDACT_MIN_LEN = 8


class SetupError(Exception):
    pass


# Credential selection


@dataclass
class Selection:
    files: List[Path]
    skipped: Counter


def _reject_constant(name: str):
    raise ValueError(f"{name} is not JSON")


def _load(path: Path) -> Optional[dict]:
    # The server's JSON parser refuses NaN and Infinity, so a file holding one is not loaded.
    try:
        data = json.loads(path.read_bytes(), parse_constant=_reject_constant)
    except (OSError, ValueError):
        return None
    return data if isinstance(data, dict) else None


def _type_of(meta: dict) -> str:
    # The loader reads `type` as a string, trimmed and lower-cased.
    value = meta.get("type")
    return value.strip().lower() if isinstance(value, str) else ""


def provider_of(type_value: str) -> Optional[str]:
    for name, types in PROVIDERS.items():
        if type_value in types:
            return name
    return None


_RFC3339 = re.compile(r"^(\d{4})-(\d{2})-(\d{2})[Tt ](\d{2}):(\d{2}):(\d{2})(\.\d+)?([Zz]|[+-]\d{2}:\d{2})$")


def _parse_rfc3339(text: str) -> Optional[datetime]:
    match = _RFC3339.match(text)
    if not match:
        return None
    year, month, day, hour, minute, second = (int(match.group(i)) for i in range(1, 7))
    zone = match.group(8)
    if zone in ("Z", "z"):
        offset = timedelta(0)
    else:
        sign = 1 if zone[0] == "+" else -1
        offset = sign * timedelta(hours=int(zone[1:3]), minutes=int(zone[4:6]))
    try:
        return datetime(year, month, day, hour, minute, second, tzinfo=timezone(offset)).astimezone(timezone.utc)
    except ValueError:
        return None


def _from_unix(value: int) -> Optional[datetime]:
    seconds = value // 1000 if value > 1_000_000_000_000 else value
    if seconds <= 0:
        return None
    try:
        return datetime.fromtimestamp(seconds, tz=timezone.utc)
    except (OverflowError, OSError, ValueError):
        return None


def _parse_time(value) -> Optional[datetime]:
    if isinstance(value, bool):
        return None
    if isinstance(value, (int, float)):
        return _from_unix(int(value)) if math.isfinite(value) else None
    if isinstance(value, str):
        text = value.strip()
        if re.fullmatch(r"[+-]?[0-9]{1,20}", text):
            return _from_unix(int(text))
        parsed = _parse_rfc3339(text)
        if parsed:
            return parsed
        for fmt in ("%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"):
            try:
                return datetime.strptime(text, fmt).replace(tzinfo=timezone.utc)
            except ValueError:
                pass
    return None


def _jwt_exp(token) -> Optional[datetime]:
    if not isinstance(token, str):
        return None
    parts = token.strip().split(".")
    if len(parts) != 3:
        return None
    try:
        payload = parts[1]
        claims = json.loads(base64.urlsafe_b64decode(payload + "=" * (-len(payload) % 4)))
    except (ValueError, TypeError):
        return None
    exp = claims.get("exp") if isinstance(claims, dict) else None
    if isinstance(exp, bool):
        return None
    if isinstance(exp, str):
        try:
            exp = int(exp.strip())
        except ValueError:
            return None
    if not isinstance(exp, (int, float)) or not math.isfinite(exp) or exp <= 0:
        return None
    return _from_unix(int(exp))


def expiry(provider: str, meta: dict) -> Optional[datetime]:
    """The access token's expiry as the server reads it, or None when unreadable."""
    if provider == "claude":
        value = meta.get("expired")
        return _parse_rfc3339(value.strip()) if isinstance(value, str) else None
    found = _jwt_exp(meta.get("access_token"))
    if found:
        return found
    for key in ("expired", "expire", "expires_at", "expiresAt", "expiry", "expires"):
        found = _parse_time(meta.get(key))
        if found:
            return found
    return None


def _auth_files(src_dir: Path) -> List[Path]:
    try:
        entries = list(src_dir.iterdir())
    except OSError:
        return []
    return sorted(p for p in entries if p.name.lower().endswith(".json") and p.is_file())


def select(src_dir: Path, provider: str, now: datetime) -> Selection:
    files: List[Path] = []
    skipped: Counter = Counter()
    for path in _auth_files(src_dir):
        meta = _load(path)
        if meta is None or provider_of(_type_of(meta)) != provider:
            continue
        if meta.get("disabled") is True:
            skipped["disabled"] += 1
            continue
        expires = expiry(provider, meta)
        if expires is not None and expires <= now + EXPIRY_MARGIN:
            skipped["token expired"] += 1
            continue
        files.append(path)
    return Selection(files, skipped)


def count_not_covered(src_dir: Path) -> int:
    count = 0
    for path in _auth_files(src_dir):
        meta = _load(path)
        if meta is not None and _type_of(meta) and provider_of(_type_of(meta)) is None:
            count += 1
    return count


# Strip, copy and verify


def _is_refresh_key(key) -> bool:
    return isinstance(key, str) and key.lower().replace("_", "") == "refreshtoken"


def strip(value):
    if isinstance(value, dict):
        return {k: strip(v) for k, v in value.items() if not _is_refresh_key(k)}
    if isinstance(value, list):
        return [strip(v) for v in value]
    return value


def _refresh_values(value, out: list) -> list:
    if isinstance(value, dict):
        for k, v in value.items():
            if _is_refresh_key(k) and v is not None and not isinstance(v, (dict, list)):
                text = v if isinstance(v, str) else json.dumps(v)
                if text:
                    out.append(text)
            _refresh_values(v, out)
    elif isinstance(value, list):
        for v in value:
            _refresh_values(v, out)
    return out


def _has_refresh_key(value) -> bool:
    if isinstance(value, dict):
        return any(_is_refresh_key(k) or _has_refresh_key(v) for k, v in value.items())
    if isinstance(value, list):
        return any(_has_refresh_key(v) for v in value)
    return False


def _strings(value, out: list) -> list:
    if isinstance(value, str):
        out.append(value)
    elif isinstance(value, dict):
        for k, v in value.items():
            out.append(k)
            _strings(v, out)
    elif isinstance(value, list):
        for v in value:
            _strings(v, out)
    return out


def _private_dir(path: Path) -> None:
    path.mkdir(mode=0o700)
    os.chmod(path, 0o700)


def copy_stripped(files: List[Path], dest_dir: Path) -> None:
    _private_dir(dest_dir)
    for src in files:
        meta = _load(src)
        if meta is None:
            raise SetupError("an auth file could not be read twice in a row")
        target = dest_dir / src.name
        fd = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "w") as out:
            json.dump(strip(meta), out)


def verify_copies(originals: List[Path], dest_dir: Path) -> None:
    """Raise SetupError unless every copy is free of refresh-token keys and values."""
    for src in originals:
        original = _load(src) or {}
        copy = dest_dir / src.name
        try:
            raw = copy.read_bytes()
            parsed = json.loads(raw)
        except (OSError, ValueError):
            raise SetupError("a stripped copy is missing or unreadable") from None
        if _has_refresh_key(parsed):
            raise SetupError("a stripped copy still holds a refresh-token key")
        # Raw bytes catch a value in any position; decoded strings catch an escaped one.
        text = raw.decode("utf-8", "replace")
        decoded = _strings(parsed, [])
        for value in _refresh_values(original, []):
            if value in text or any(value in s for s in decoded):
                raise SetupError("a stripped copy still holds a refresh-token value")


# Server child


def child_env(caller_env, run_dir: Path) -> dict:
    env = {name: caller_env[name] for name in CHILD_ENV_ALLOW if name in caller_env}
    env["WRITABLE_PATH"] = str(run_dir)
    return env


def config_doc(port: int, client_key: str, auth_dir: Path) -> dict:
    return {
        "config-version": 8,
        "server": {"host": "127.0.0.1", "port": port},
        "access": {"api-keys": [client_key]},
        "oauth": {"auth-dir": str(auth_dir)},
        "routing": {"retry": {"request-retry": 0, "max-retry-credentials": 1}},
        "observability": {
            "logs": {"request-log": False, "logging-to-file": False},
            "history": {"enable": False},
        },
    }


def config_text(port: int, client_key: str, auth_dir: Path) -> str:
    # JSON is a subset of the YAML the config loader reads.
    return json.dumps(config_doc(port, client_key, auth_dir), indent=2) + "\n"


def pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def make_run_dir(tmp: Path) -> Path:
    run = Path(tempfile.mkdtemp(prefix=RUN_PREFIX, dir=str(tmp)))
    os.chmod(run, 0o700)
    (run / "pid").write_text(str(os.getpid()))
    return run


def clean_leftovers(tmp: Path) -> None:
    """Delete run directories whose recorded process is gone; leave every other one."""
    for path in tmp.glob(RUN_PREFIX + "*"):
        try:
            if not path.is_dir() or path.is_symlink() or path.stat().st_uid != os.getuid():
                continue
            pid = int((path / "pid").read_text().strip())
        except (OSError, ValueError):
            continue
        if pid != os.getpid() and not pid_alive(pid):
            shutil.rmtree(path, ignore_errors=True)


# Requests and replies


def _sse_events(body: bytes):
    """Yield (event name, data text) for each data line of an event stream."""
    name = None
    for raw in body.decode("utf-8", "replace").splitlines():
        line = raw.rstrip("\r")
        if line.startswith("event:"):
            name = line[6:].strip()
        elif line.startswith("data:"):
            yield name, line[5:].strip()
        elif not line:
            name = None


def _json(text):
    try:
        return json.loads(text)
    except ValueError:
        return None


def _field(value, key):
    """`value[key]` when `value` is an object, else None."""
    return value.get(key) if isinstance(value, dict) else None


def _items(value) -> list:
    """`value` when it is an array, else nothing to iterate."""
    return value if isinstance(value, list) else []


def _responses_text(response) -> str:
    if not isinstance(response, dict):
        return ""
    text = response.get("output_text") if isinstance(response.get("output_text"), str) else ""
    for item in _items(response.get("output")):
        if isinstance(item, dict):
            for part in _items(item.get("content")):
                if isinstance(part, dict) and part.get("type") == "output_text" and isinstance(part.get("text"), str):
                    text += part["text"]
    return text


def _messages_text(message) -> str:
    if not isinstance(message, dict):
        return ""
    return "".join(
        part.get("text", "")
        for part in _items(message.get("content"))
        if isinstance(part, dict) and part.get("type") == "text" and isinstance(part.get("text"), str)
    )


def _chat_text(reply) -> str:
    if not isinstance(reply, dict):
        return ""
    text = ""
    for choice in _items(reply.get("choices")):
        content = _field(_field(choice, "message"), "content")
        if isinstance(content, str):
            text += content
    return text


def check_reply(route: str, stream: bool, status: int, body: bytes) -> Optional[str]:
    """None when the reply passes, else the reason it does not."""
    if status != 200:
        return f"{status} {body.decode('utf-8', 'replace')}"
    if not stream:
        reply = _json(body)
        text = {
            "/v1/messages": _messages_text,
            "/v1/chat/completions": _chat_text,
            "/v1/responses": _responses_text,
        }[route](reply)
        return None if text.strip() else "200 without text"
    text, done = "", False
    for name, data in _sse_events(body):
        if route == "/v1/chat/completions":
            if data == "[DONE]":
                done = True
                continue
            chunk = _json(data)
            for choice in _items(_field(chunk, "choices")):
                content = _field(_field(choice, "delta"), "content")
                if isinstance(content, str):
                    text += content
            continue
        event = _json(data)
        kind = event.get("type") if isinstance(event, dict) else None
        if route == "/v1/messages":
            if kind == "content_block_delta":
                delta = event.get("delta")
                if _field(delta, "type") == "text_delta" and isinstance(_field(delta, "text"), str):
                    text += delta["text"]
            elif kind == "message_stop" or name == "message_stop":
                done = True
        else:
            if kind == "response.output_text.delta" and isinstance(event.get("delta"), str):
                text += event["delta"]
            elif kind == "response.completed":
                done = True
                if not text.strip():
                    text = _responses_text(event.get("response"))
    terminator = {"/v1/messages": "message_stop", "/v1/chat/completions": "[DONE]", "/v1/responses": "response.completed"}[route]
    if not done:
        return f"200 stream ended without {terminator}"
    return None if text.strip() else "200 without text"


def pick_model(provider: str, models_body: bytes) -> Optional[str]:
    reply = _json(models_body)
    ids = [
        m.get("id")
        for m in _items(_field(reply, "data"))
        if isinstance(m, dict) and isinstance(m.get("id"), str)
    ]
    usable = [i for i in ids if not any(word in i.lower() for word in MODEL_EXCLUDE)]
    for wanted in MODEL_PREFERENCE[provider]:
        matches = sorted((i for i in usable if wanted in i.lower()), key=lambda i: (len(i), i))
        if matches:
            return matches[0]
    return None


def request_body(route: str, model: str, stream: bool) -> dict:
    if route in ("/v1/messages", "/v1/chat/completions"):
        return {"model": model, "max_tokens": OUTPUT_CAP, "stream": stream, "messages": [{"role": "user", "content": PROMPT}]}
    return {"model": model, "max_output_tokens": OUTPUT_CAP, "stream": stream, "input": PROMPT}


# Report


@dataclass
class Outcome:
    route: str
    stream: bool
    status: int
    reason: Optional[str]


@dataclass
class ProviderResult:
    provider: str
    accounts: int
    skipped: Counter
    outcomes: List[Outcome]
    error: Optional[str] = None


class Redactor:
    """Removes the selected files' names and identifying values from printed text."""

    def __init__(self, files: List[Path]):
        found = set()
        for path in files:
            found.update(n for n in (path.name, path.stem) if len(n) >= REDACT_MIN_LEN)
            meta = _load(path)
            if meta is not None:
                self._collect(meta, found)
        self.secrets = sorted(found, key=len, reverse=True)

    def _collect(self, value, found: set, key=None) -> None:
        if isinstance(value, dict):
            for k, v in value.items():
                self._collect(v, found, k)
        elif isinstance(value, list):
            for v in value:
                self._collect(v, found, key)
        elif isinstance(value, str) and key not in REDACT_SKIP_KEYS and len(value) >= REDACT_MIN_LEN:
            found.add(value)

    def add(self, files: List[Path]) -> None:
        self.secrets = sorted(set(self.secrets) | set(Redactor(files).secrets), key=len, reverse=True)

    def clean(self, text: str) -> str:
        for secret in self.secrets:
            text = text.replace(secret, "<redacted>")
        return " ".join(text.split())


def _skips(skipped: Counter) -> str:
    return ", ".join(f"{n} {reason}" for reason, n in sorted(skipped.items()))


def _excerpt(reason: str, redactor: Redactor) -> str:
    """The start of a failure reason, cut only after redaction so no part of a value prints."""
    return redactor.clean(reason)[:EXCERPT]


def report_line(result: ProviderResult, redactor: Redactor) -> str:
    name = result.provider
    if result.accounts == 0:
        return f"{name}: skipped ({_skips(result.skipped) or 'no credentials'})"
    tail = f", {result.accounts} account{'s' if result.accounts != 1 else ''}"
    if result.skipped:
        tail += f", skipped ({_skips(result.skipped)})"
    if result.error is not None:
        return f"{name}: FAIL {_excerpt(result.error, redactor)}{tail}"
    passed = sum(1 for o in result.outcomes if o.reason is None)
    total = len(result.outcomes)
    failures = [o for o in result.outcomes if o.reason is not None]
    if not failures:
        return f"{name}: pass ({passed}/{total}){tail}"
    first = failures[0]
    label = first.route + (" stream" if first.stream else "")
    line = f"{name}: FAIL ({passed}/{total}) {label}: {_excerpt(first.reason, redactor)}"
    if any(o.status == 401 for o in failures):
        line += " (the installed service may have refreshed the account during the run; a rerun tells the two apart)"
    return line + tail


# Run


class Server:
    def __init__(self, binary: Path, run_dir: Path, auth_dir: Path):
        self.port = _free_port()
        self.key = secrets.token_hex(24)
        self.base = f"http://127.0.0.1:{self.port}"
        work = run_dir / "server"
        _private_dir(work)
        config = work / "config.yaml"
        config.write_text(config_text(self.port, self.key, auth_dir))
        os.chmod(config, 0o600)
        self.log_path = work / "server.log"
        self._log = open(self.log_path, "wb")
        self.proc = subprocess.Popen(
            [str(binary), "--config", str(config), "--working-dir", str(work), "--local-model"],
            cwd=str(work),
            env=child_env(os.environ, run_dir),
            stdin=subprocess.DEVNULL,
            stdout=self._log,
            stderr=subprocess.STDOUT,
        )
        self.work = work

    def wait_healthy(self) -> bool:
        deadline = time.monotonic() + HEALTH_TIMEOUT
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                return False
            try:
                with _OPENER.open(self.base + "/healthz", timeout=2) as resp:
                    if resp.status == 200:
                        return True
            except TRANSPORT_ERRORS:
                pass
            time.sleep(0.25)
        return False

    def log_tail(self, lines: int = 20) -> str:
        try:
            return "\n".join(self.log_path.read_text("utf-8", "replace").splitlines()[-lines:])
        except OSError:
            return ""

    def call(self, method: str, path: str, body: Optional[dict] = None):
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(self.base + path, data=data, method=method)
        req.add_header("Authorization", f"Bearer {self.key}")
        # Only the Messages route gets it: with it, /v1/models answers in Claude form and
        # disguises every other provider's model ids.
        if path == "/v1/messages":
            req.add_header("anthropic-version", "2023-06-01")
        if data is not None:
            req.add_header("Content-Type", "application/json")
        try:
            with _OPENER.open(req, timeout=REQUEST_TIMEOUT) as resp:
                return resp.status, resp.read()
        except urllib.error.HTTPError as err:
            return err.code, err.read()

    def stop(self) -> None:
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self._log.close()
        shutil.rmtree(self.work, ignore_errors=True)


# A refused connection, a timeout, or a reply that broke off (`IncompleteRead` is not an
# `OSError`).
TRANSPORT_ERRORS = (OSError, http.client.HTTPException)

# Requests to the local server must not go through a caller's HTTP proxy.
_OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def check_provider(server: Server, provider: str) -> tuple:
    """(outcomes, error) for one provider's six requests."""
    try:
        status, body = server.call("GET", "/v1/models")
    except TRANSPORT_ERRORS as err:
        return [], f"/v1/models: transport error {type(err).__name__}"
    if status != 200:
        return [], f"/v1/models: {status} {body.decode('utf-8', 'replace')}"
    model = pick_model(provider, body)
    if model is None:
        return [], "no usable model in /v1/models"
    outcomes = []
    for route, stream in ROUTES:
        try:
            status, reply = server.call("POST", route, request_body(route, model, stream))
            reason = check_reply(route, stream, status, reply)
        except TRANSPORT_ERRORS as err:
            status, reason = 0, f"transport error {type(err).__name__}"
        outcomes.append(Outcome(route, stream, status, reason))
    return outcomes, None


def _build() -> Path:
    env = dict(os.environ, CARGO_BUILD_JOBS="2")
    try:
        result = subprocess.run(["cargo", "build", "-p", "cliproxy", "--locked"], cwd=str(REPO), env=env)
    except OSError as err:
        raise SetupError(f"cargo could not start: {type(err).__name__}") from err
    if result.returncode != 0:
        raise SetupError("cargo build failed")
    target = Path(os.environ.get("CARGO_TARGET_DIR") or REPO / "target")
    if not target.is_absolute():
        target = REPO / target
    binary = target / "debug" / ("cliproxy.exe" if os.name == "nt" else "cliproxy")
    if not binary.is_file():
        raise SetupError(f"built binary not found at {binary}")
    return binary


def _auth_dir(arg: Optional[str]) -> Path:
    raw = arg or os.environ.get("CLIPROXY_LIVE_AUTH_DIR") or "~/.cli-proxy-api"
    return Path(raw).expanduser()


def _raise_exit(signum, _frame):
    raise SystemExit(128 + signum)


def run(argv: Optional[List[str]] = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--auth-dir", help="source of auth files (read only); default $CLIPROXY_LIVE_AUTH_DIR, then ~/.cli-proxy-api")
    parser.add_argument("--provider", choices=sorted(PROVIDERS), help="check only this provider")
    args = parser.parse_args(argv)

    src = _auth_dir(args.auth_dir)
    if not src.is_dir():
        print("setup failed: the auth directory does not exist", file=sys.stderr)
        return 2
    try:
        binary = _build()
    except SetupError as err:
        print(f"setup failed: {err}", file=sys.stderr)
        return 2

    tmp = Path(tempfile.gettempdir())
    clean_leftovers(tmp)
    run_dir = make_run_dir(tmp)
    server: Optional[Server] = None
    for signum in (signal.SIGINT, signal.SIGTERM):
        signal.signal(signum, _raise_exit)
    redactor = Redactor([])
    checked = failed = False
    try:
        for provider in [args.provider] if args.provider else list(PROVIDERS):
            selection = select(src, provider, datetime.now(timezone.utc))
            redactor.add(selection.files)
            result = ProviderResult(provider, len(selection.files), selection.skipped, [])
            if selection.files:
                auth = run_dir / f"auth-{provider}"
                copy_stripped(selection.files, auth)
                verify_copies(selection.files, auth)
                server = Server(binary, run_dir, auth)
                if not server.wait_healthy():
                    tail = redactor.clean(server.log_tail())
                    raise SetupError(f"the {provider} server did not answer /healthz within {HEALTH_TIMEOUT:.0f} s; log tail: {tail}")
                result.outcomes, result.error = check_provider(server, provider)
                server.stop()
                server = None
                shutil.rmtree(auth, ignore_errors=True)
                checked = True
                failed = failed or result.error is not None or any(o.reason for o in result.outcomes)
            print(report_line(result, redactor), flush=True)
        if not args.provider:
            other = count_not_covered(src)
            if other:
                print(f"not covered: {other} credential{'s' if other != 1 else ''} of other types")
    except (SetupError, OSError) as err:
        print(f"setup failed: {redactor.clean(str(err))}", file=sys.stderr)
        return 2
    finally:
        if server is not None:
            server.stop()
        shutil.rmtree(run_dir, ignore_errors=True)
    if failed:
        return 1
    return 0 if checked else 3


if __name__ == "__main__":
    sys.exit(run())
