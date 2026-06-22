#!/usr/bin/env python3
"""Policy wrapper that exposes Browser Use as a stdio MCP server."""

from __future__ import annotations

import argparse
import asyncio
import hashlib
import json
import os
import re
import shutil
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import urlparse

DEFAULT_AUDIT_LOG = "~/.local/share/acp-stack/browser-use-mcp-audit.jsonl"
DEFAULT_DOWNLOAD_DIR = "/workspace/browser-downloads"
DEFAULT_MAX_RESULT_CHARS = 12000
DEFAULT_MAX_TASK_CHARS = 4000
DEFAULT_TIMEOUT_SECS = 300

CREDENTIAL_TERMS = (
    "api key",
    "credential",
    "login",
    "log in",
    "password",
    "session token",
    "sign in",
    "signin",
    "two-factor",
    "2fa",
)

PAYMENT_TERMS = (
    "bank account",
    "checkout",
    "credit card",
    "payment",
    "purchase",
    "wire transfer",
)

URL_RE = re.compile(r"https?://[^\s)'\"<>]+")


@dataclass(frozen=True)
class Policy:
    allowed_domains: tuple[str, ...]
    allow_any_domain: bool
    allow_credentials: bool
    allow_payments: bool
    audit_log: Path
    browser_executable: str
    download_dir: Path
    max_result_chars: int
    max_task_chars: int
    timeout_secs: int


POLICY: Policy | None = None


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run a policy-gated Browser Use stdio MCP server."
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="Run wrapper policy self-tests and exit.",
    )
    parser.add_argument(
        "--allowed-domain",
        action="append",
        default=[],
        help="Allowed domain suffix. Repeat for more domains.",
    )
    parser.add_argument(
        "--allow-any-domain",
        action="store_true",
        help="Permit tasks to target any domain.",
    )
    parser.add_argument(
        "--allow-credentials",
        action="store_true",
        help="Permit tasks that mention credentialed login or account access.",
    )
    parser.add_argument(
        "--allow-payments",
        action="store_true",
        help="Permit tasks that mention purchases or payment flows.",
    )
    parser.add_argument(
        "--audit-log",
        default=DEFAULT_AUDIT_LOG,
        help=f"JSONL audit log path (default: {DEFAULT_AUDIT_LOG}).",
    )
    parser.add_argument(
        "--browser-executable",
        default=os.environ.get("BROWSER_USE_CHROMIUM"),
        help="Chromium executable path. Defaults to chromium/chromium-browser on PATH.",
    )
    parser.add_argument(
        "--download-dir",
        default=DEFAULT_DOWNLOAD_DIR,
        help=f"Directory for browser-created files (default: {DEFAULT_DOWNLOAD_DIR}).",
    )
    parser.add_argument(
        "--max-result-chars",
        type=int,
        default=DEFAULT_MAX_RESULT_CHARS,
        help=f"Maximum result text returned to the agent (default: {DEFAULT_MAX_RESULT_CHARS}).",
    )
    parser.add_argument(
        "--max-task-chars",
        type=int,
        default=DEFAULT_MAX_TASK_CHARS,
        help=f"Maximum task prompt length (default: {DEFAULT_MAX_TASK_CHARS}).",
    )
    parser.add_argument(
        "--timeout-secs",
        type=int,
        default=DEFAULT_TIMEOUT_SECS,
        help=f"Browser task timeout in seconds (default: {DEFAULT_TIMEOUT_SECS}).",
    )
    return parser.parse_args()


def build_policy(args: argparse.Namespace) -> Policy:
    domains = tuple(normalize_domain(domain) for domain in args.allowed_domain)
    if not domains and not args.allow_any_domain:
        raise SystemExit(
            "at least one --allowed-domain is required unless --allow-any-domain is set"
        )
    if "BROWSER_USE_API_KEY" not in os.environ:
        raise SystemExit("BROWSER_USE_API_KEY must be provided through the MCP env secret refs")
    if args.max_result_chars <= 0:
        raise SystemExit("--max-result-chars must be positive")
    if args.max_task_chars <= 0:
        raise SystemExit("--max-task-chars must be positive")
    if args.timeout_secs <= 0:
        raise SystemExit("--timeout-secs must be positive")

    audit_log = Path(args.audit_log).expanduser()
    browser_executable = resolve_browser_executable(args.browser_executable)
    if not browser_executable:
        raise SystemExit("Chromium executable not found; install the browser VM profile")
    download_dir = Path(args.download_dir).expanduser()
    if not download_dir.is_absolute():
        raise SystemExit("--download-dir must be absolute")

    download_dir.mkdir(parents=True, exist_ok=True)
    audit_log.parent.mkdir(parents=True, exist_ok=True)

    return Policy(
        allowed_domains=domains,
        allow_any_domain=args.allow_any_domain,
        allow_credentials=args.allow_credentials,
        allow_payments=args.allow_payments,
        audit_log=audit_log,
        browser_executable=browser_executable,
        download_dir=download_dir,
        max_result_chars=args.max_result_chars,
        max_task_chars=args.max_task_chars,
        timeout_secs=args.timeout_secs,
    )


def normalize_domain(value: str) -> str:
    domain = value.strip().lower()
    if not domain:
        raise SystemExit("--allowed-domain must not be empty")
    if "://" in domain:
        parsed = urlparse(domain)
        domain = parsed.hostname or ""
    domain = domain.strip(".")
    if not domain or "/" in domain:
        raise SystemExit(f"invalid --allowed-domain value: {value}")
    return domain


def find_chromium() -> str | None:
    return shutil.which("chromium") or shutil.which("chromium-browser")


def resolve_browser_executable(value: str | None) -> str | None:
    if not value:
        return find_chromium()
    candidate = shutil.which(value) if "/" not in value else value
    if candidate and os.access(candidate, os.X_OK):
        return candidate
    raise SystemExit(f"browser executable is not executable: {value}")


def sample_policy(**overrides: object) -> Policy:
    values = {
        "allowed_domains": ("example.com",),
        "allow_any_domain": False,
        "allow_credentials": False,
        "allow_payments": False,
        "audit_log": Path("/tmp/acp-stack-browser-use-audit.jsonl"),
        "browser_executable": sys.executable,
        "download_dir": Path("/tmp/acp-stack-browser-downloads"),
        "max_result_chars": 12000,
        "max_task_chars": 4000,
        "timeout_secs": 300,
    }
    values.update(overrides)
    return Policy(**values)


def expect_value_error(callback) -> None:
    try:
        callback()
    except ValueError:
        return
    raise AssertionError("expected ValueError")


def expect_system_exit(callback) -> None:
    try:
        callback()
    except SystemExit:
        return
    raise AssertionError("expected SystemExit")


def run_self_test() -> None:
    policy = sample_policy()
    assert normalize_domain("https://Docs.Example.com/path") == "docs.example.com"
    assert browser_profile_domains(policy) == ["example.com", "*.example.com"]
    assert browser_profile_domains(sample_policy(allow_any_domain=True)) is None
    assert assert_allowed("Read https://docs.example.com/page", None, policy) == [
        "docs.example.com"
    ]
    expect_value_error(lambda: assert_allowed("Read https://bad.example.net", None, policy))
    expect_value_error(lambda: assert_allowed("Log in to https://example.com", None, policy))
    expect_value_error(lambda: assert_allowed("Purchase from https://example.com", None, policy))
    assert resolve_browser_executable(sys.executable) == sys.executable
    expect_system_exit(lambda: resolve_browser_executable("/definitely/missing/browser"))


def current_policy() -> Policy:
    if POLICY is None:
        raise RuntimeError("Browser Use MCP policy was not initialized")
    return POLICY


def explicit_urls(task: str, start_url: str | None) -> list[str]:
    urls = URL_RE.findall(task)
    if start_url:
        urls.append(start_url)
    return urls


def domain_allowed(host: str, allowed_domains: tuple[str, ...]) -> bool:
    normalized = host.lower().strip(".")
    return any(
        normalized == domain or normalized.endswith(f".{domain}")
        for domain in allowed_domains
    )


def assert_allowed(task: str, start_url: str | None, policy: Policy) -> list[str]:
    if len(task) > policy.max_task_chars:
        raise ValueError(f"task is longer than {policy.max_task_chars} characters")

    lowered = task.casefold()
    if not policy.allow_credentials and any(term in lowered for term in CREDENTIAL_TERMS):
        raise ValueError("credentialed browser tasks require --allow-credentials")
    if not policy.allow_payments and any(term in lowered for term in PAYMENT_TERMS):
        raise ValueError("payment browser tasks require --allow-payments")

    hosts = []
    for url in explicit_urls(task, start_url):
        parsed = urlparse(url)
        if parsed.scheme not in {"http", "https"} or not parsed.hostname:
            raise ValueError(f"invalid URL in browser task: {url}")
        host = parsed.hostname.lower().strip(".")
        hosts.append(host)
        if not policy.allow_any_domain and not domain_allowed(host, policy.allowed_domains):
            allowed = ", ".join(policy.allowed_domains)
            raise ValueError(f"domain {host} is outside allowed domains: {allowed}")
    return hosts


def audit(event: dict[str, object], policy: Policy) -> None:
    payload = {"ts": time.time(), **event}
    with policy.audit_log.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(payload, sort_keys=True) + "\n")


def compose_task(task: str, start_url: str | None, policy: Policy) -> str:
    allowed = "any domain" if policy.allow_any_domain else ", ".join(policy.allowed_domains)
    lines = [
        "Run this browser task under these policy limits:",
        f"- Allowed domains: {allowed}",
        f"- Write downloads and generated files only under: {policy.download_dir}",
        "- Do not enter credentials unless the task explicitly asks and policy allows it.",
        "- Do not make purchases or payments unless the task explicitly asks and policy allows it.",
    ]
    if start_url:
        lines.append(f"- Start URL: {start_url}")
    lines.append("")
    lines.append(task)
    return "\n".join(lines)


def browser_profile_domains(policy: Policy) -> list[str] | None:
    if policy.allow_any_domain:
        return None
    out = []
    for domain in policy.allowed_domains:
        out.append(domain)
        out.append(f"*.{domain}")
    return out


async def run_browser_task(task: str, start_url: str | None = None) -> dict[str, object]:
    """Run one Browser Use task after applying local policy checks."""

    policy = current_policy()
    task_hash = hashlib.sha256(task.encode("utf-8")).hexdigest()
    hosts = assert_allowed(task, start_url, policy)
    audit(
        {
            "event": "browser_task_started",
            "hosts": hosts,
            "task_sha256": task_hash,
        },
        policy,
    )

    try:
        from browser_use import Agent, BrowserProfile, ChatBrowserUse
    except ImportError as err:
        raise RuntimeError(
            "browser-use is not installed; run scripts/install-agent-vm-deps.sh --profile browser"
        ) from err

    browser_profile = BrowserProfile(
        allowed_domains=browser_profile_domains(policy),
        downloads_path=policy.download_dir,
        executable_path=policy.browser_executable,
        headless=True,
    )
    agent = Agent(
        task=compose_task(task, start_url, policy),
        llm=ChatBrowserUse(),
        browser_profile=browser_profile,
    )
    try:
        history = await asyncio.wait_for(agent.run(), timeout=policy.timeout_secs)
    except Exception as err:
        audit(
            {
                "event": "browser_task_failed",
                "error": type(err).__name__,
                "hosts": hosts,
                "task_sha256": task_hash,
            },
            policy,
        )
        raise

    result = str(history)
    truncated = len(result) > policy.max_result_chars
    if truncated:
        result = result[-policy.max_result_chars :]
    audit(
        {
            "event": "browser_task_completed",
            "hosts": hosts,
            "result_chars": len(result),
            "result_truncated": truncated,
            "task_sha256": task_hash,
        },
        policy,
    )
    return {
        "status": "completed",
        "allowed_domains": list(policy.allowed_domains),
        "download_dir": str(policy.download_dir),
        "result": result,
        "result_truncated": truncated,
    }


def main() -> None:
    global POLICY
    args = parse_args()
    if args.self_test:
        run_self_test()
        return

    POLICY = build_policy(args)
    from mcp.server.fastmcp import FastMCP

    mcp = FastMCP("browser-use")
    mcp.tool()(run_browser_task)
    mcp.run()


if __name__ == "__main__":
    try:
        main()
    except BrokenPipeError:
        sys.exit(1)
