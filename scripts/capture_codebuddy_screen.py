#!/usr/bin/env python3
"""Capture account-free CodeBuddy screen fixtures in a tmux PTY.

The harness starts a local mock model server, points CodeBuddy at it with
CODEBUDDY_API_KEY and CODEBUDDY_BASE_URL, runs CodeBuddy in tmux with isolated
HOME and CODEBUDDY_CONFIG_DIR directories under .local, captures the workspace
trust prompt, accepts it, captures the idle prompt-box screen, captures menu
skip screens, then delays a mock Bash tool-call long enough to capture working
and permission fixtures as text and ANSI.

Example:
    python3 scripts/capture_codebuddy_screen.py --fixture-dir tests/fixtures/agent-screen/codebuddy
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import threading
import time
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Callable


PROJECT_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_OUT_DIR = PROJECT_ROOT / ".local/codebuddy-screen-captures"
IDLE_PROMPT_RE = re.compile(r"(?m)^─{3,}.*\n>\s*\n─{3,}")
OSC_WORKING_TITLE_RE = re.compile(r"^[\u2800-\u28ff] ")
TRUST_PROMPT = "Do you trust the files in this folder?"
PERMISSION_PROMPT = "Do you want to proceed?"
PERMISSION_USER_PROMPT = "Please run echo herdr-permission"
PERMISSION_COMMAND = "echo herdr-permission"
PERMISSION_COMMAND_DESCRIPTION = "Print permission fixture marker"
TITLE_RESPONSE = '{"isNewTopic": true, "title": "Run permission command"}'
WORKING_INDICATOR = "esc to interrupt"
MODEL_PICKER_PROMPT = "/model"
MODEL_PICKER_MARKER = "Select Model"
TRANSCRIPT_MARKER = "Showing detailed transcript"
FIXTURE_FILES = (
    "trust.txt",
    "trust.ansi",
    "idle.txt",
    "idle.ansi",
    "model-picker.txt",
    "model-picker.ansi",
    "model-picker-session.txt",
    "model-picker-session.ansi",
    "working.txt",
    "working.ansi",
    "working-osc-title.txt",
    "permission.txt",
    "permission.ansi",
    "transcript-viewer.txt",
    "transcript-viewer.ansi",
)


class MockModelHandler(BaseHTTPRequestHandler):
    server: "MockModelServer"

    def do_GET(self) -> None:
        self.server.record(self.command, self.path, b"")
        if self.path.rstrip("/") in {"", "/health"}:
            self.write_json({"ok": True})
            return
        if self.path.rstrip("/") in {"/models", "/v1/models"}:
            self.write_json({"object": "list", "data": [{"id": "mock-codebuddy", "object": "model"}]})
            return
        self.write_json({"ok": True})

    def do_POST(self) -> None:
        body = self.rfile.read(int(self.headers.get("Content-Length", "0") or "0"))
        self.server.record(self.command, self.path, body)
        body_json = parse_json_object(body)
        if self.path.endswith("/messages"):
            self.write_json(
                {
                    "id": "msg_mock",
                    "type": "message",
                    "role": "assistant",
                    "model": "mock-codebuddy",
                    "content": [{"type": "text", "text": "mock response"}],
                    "stop_reason": "end_turn",
                    "stop_sequence": None,
                    "usage": {"input_tokens": 1, "output_tokens": 1},
                }
            )
            return
        if self.path.endswith("/chat/completions"):
            if body_json.get("stream"):
                if body_json.get("tools"):
                    delay = self.server.consume_tool_stream_delay()
                    if delay > 0.0:
                        time.sleep(delay)
                    self.write_openai_bash_tool_call_stream()
                else:
                    self.write_openai_text_stream(TITLE_RESPONSE)
                return
            self.write_json(
                {
                    "id": "chatcmpl_mock",
                    "object": "chat.completion",
                    "choices": [
                        {
                            "index": 0,
                            "message": {"role": "assistant", "content": "mock response"},
                            "finish_reason": "stop",
                        }
                    ],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
                }
            )
            return
        self.write_json({"ok": True})

    def log_message(self, _format: str, *_args: Any) -> None:
        return

    def write_json(self, value: dict[str, Any]) -> None:
        payload = json.dumps(value).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def write_openai_text_stream(self, text: str) -> None:
        self.write_openai_stream(
            [
                {
                    "id": "chatcmpl_mock",
                    "object": "chat.completion.chunk",
                    "choices": [
                        {"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}
                    ],
                },
                {
                    "id": "chatcmpl_mock",
                    "object": "chat.completion.chunk",
                    "choices": [
                        {"index": 0, "delta": {"content": text}, "finish_reason": None}
                    ],
                },
                {
                    "id": "chatcmpl_mock",
                    "object": "chat.completion.chunk",
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                },
            ]
        )

    def write_openai_bash_tool_call_stream(self) -> None:
        arguments = json.dumps(
            {
                "command": PERMISSION_COMMAND,
                "description": PERMISSION_COMMAND_DESCRIPTION,
            }
        )
        self.write_openai_stream(
            [
                {
                    "id": "chatcmpl_mock",
                    "object": "chat.completion.chunk",
                    "choices": [
                        {"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}
                    ],
                },
                {
                    "id": "chatcmpl_mock",
                    "object": "chat.completion.chunk",
                    "choices": [
                        {
                            "index": 0,
                            "delta": {
                                "tool_calls": [
                                    {
                                        "index": 0,
                                        "id": "call_herdr_permission",
                                        "type": "function",
                                        "function": {
                                            "name": "Bash",
                                            "arguments": arguments,
                                        },
                                    }
                                ]
                            },
                            "finish_reason": None,
                        }
                    ],
                },
                {
                    "id": "chatcmpl_mock",
                    "object": "chat.completion.chunk",
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
                },
            ]
        )

    def write_openai_stream(self, chunks: list[dict[str, Any]]) -> None:
        payload = (
            "".join(
                f"data: {json.dumps(chunk, separators=(',', ':'))}\n\n"
                for chunk in chunks
            )
            + "data: [DONE]\n\n"
        ).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


class MockModelServer(ThreadingHTTPServer):
    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), MockModelHandler)
        self.requests: list[dict[str, Any]] = []
        self._tool_stream_delay_seconds = 0.0
        self._lock = threading.Lock()

    @property
    def base_url(self) -> str:
        host, port = self.server_address
        return f"http://{host}:{port}"

    def record(self, method: str, path: str, body: bytes) -> None:
        try:
            body_text = body.decode("utf-8")
        except UnicodeDecodeError:
            body_text = body.hex()
        with self._lock:
            self.requests.append(
                {
                    "at": iso_now(),
                    "method": method,
                    "path": path,
                    "body": body_text,
                }
            )

    def delay_next_tool_stream(self, seconds: float) -> None:
        with self._lock:
            self._tool_stream_delay_seconds = seconds

    def consume_tool_stream_delay(self) -> float:
        with self._lock:
            seconds = self._tool_stream_delay_seconds
            self._tool_stream_delay_seconds = 0.0
            return seconds


def main() -> int:
    args = parse_args()
    require_executable("tmux")
    command = shutil.which(args.command)
    if command is None:
        print(f"error: could not find CodeBuddy command {args.command!r}", file=sys.stderr)
        return 1

    run_dir = args.out / datetime.now().strftime("%Y%m%d-%H%M%S")
    run_dir.mkdir(parents=True, exist_ok=False)
    home = run_dir / "home"
    codebuddy_config = run_dir / "codebuddy-config"
    xdg_config = run_dir / "xdg-config"
    xdg_state = run_dir / "xdg-state"
    for path in (home, codebuddy_config, xdg_config, xdg_state):
        path.mkdir(parents=True, exist_ok=True)

    server = MockModelServer()
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()

    session_name = f"herdr-codebuddy-capture-{os.getpid()}"
    pane_id: str | None = None
    try:
        pane_id = start_tmux_codebuddy(
            args=args,
            command=command,
            session_name=session_name,
            home=home,
            codebuddy_config=codebuddy_config,
            xdg_config=xdg_config,
            xdg_state=xdg_state,
            base_url=server.base_url,
        )
        time.sleep(args.startup_delay)
        capture_pane(pane_id, run_dir / "trust.txt", ansi=False)
        capture_pane(pane_id, run_dir / "trust.ansi", ansi=True)

        trust_screen = (run_dir / "trust.txt").read_text(encoding="utf-8", errors="replace")
        if TRUST_PROMPT in trust_screen:
            run(["tmux", "send-keys", "-t", pane_id, "Enter"])

        idle_screen = wait_for_idle(pane_id, args.timeout)
        (run_dir / "idle.txt").write_text(idle_screen, encoding="utf-8")
        capture_pane(pane_id, run_dir / "idle.ansi", ansi=True)

        submit_prompt(pane_id, MODEL_PICKER_PROMPT)
        model_picker_screen = wait_for_model_picker(pane_id, args.timeout)
        (run_dir / "model-picker.txt").write_text(model_picker_screen, encoding="utf-8")
        capture_pane(pane_id, run_dir / "model-picker.ansi", ansi=True)
        run(["tmux", "send-keys", "-t", pane_id, "Tab"])
        model_picker_session_screen = wait_for_model_picker_session(pane_id, args.timeout)
        (run_dir / "model-picker-session.txt").write_text(
            model_picker_session_screen,
            encoding="utf-8",
        )
        capture_pane(pane_id, run_dir / "model-picker-session.ansi", ansi=True)
        run(["tmux", "send-keys", "-t", pane_id, "Escape"])
        wait_for_idle(pane_id, args.timeout)

        server.delay_next_tool_stream(args.working_delay)
        submit_prompt(pane_id, PERMISSION_USER_PROMPT)
        working_screen = wait_for_working(pane_id, args.timeout)
        (run_dir / "working.txt").write_text(working_screen, encoding="utf-8")
        capture_pane(pane_id, run_dir / "working.ansi", ansi=True)
        (run_dir / "working-osc-title.txt").write_text(
            wait_for_working_osc_title(pane_id, min(args.working_delay, args.timeout)),
            encoding="utf-8",
        )
        permission_screen = wait_for_permission(pane_id, args.timeout)
        (run_dir / "permission.txt").write_text(permission_screen, encoding="utf-8")
        capture_pane(pane_id, run_dir / "permission.ansi", ansi=True)
        dismiss_permission_prompt(pane_id, args.timeout)

        run(["tmux", "send-keys", "-t", pane_id, "C-o"])
        transcript_screen = wait_for_transcript_viewer(pane_id, args.timeout)
        (run_dir / "transcript-viewer.txt").write_text(transcript_screen, encoding="utf-8")
        capture_pane(pane_id, run_dir / "transcript-viewer.ansi", ansi=True)
        write_metadata(
            run_dir,
            args=args,
            command=command,
            base_url=server.base_url,
            session_name=session_name,
            pane_id=pane_id,
        )
        write_requests(server, run_dir / "mock-requests.jsonl")
        if args.fixture_dir:
            args.fixture_dir.mkdir(parents=True, exist_ok=True)
            for name in FIXTURE_FILES:
                shutil.copyfile(run_dir / name, args.fixture_dir / name)
        print(f"captured CodeBuddy screens under {run_dir}")
        return 0
    finally:
        if pane_id and not args.keep_session:
            run(["tmux", "kill-session", "-t", session_name], check=False)
        server.shutdown()
        server.server_close()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--command", default="codebuddy", help="CodeBuddy command to run")
    parser.add_argument("--cwd", type=Path, default=PROJECT_ROOT, help="working directory for CodeBuddy")
    parser.add_argument("--out", type=Path, default=DEFAULT_OUT_DIR, help="capture output directory")
    parser.add_argument(
        "--fixture-dir",
        type=Path,
        help="optional directory to refresh with text and ANSI fixtures",
    )
    parser.add_argument("--cols", type=int, default=100, help="tmux PTY columns")
    parser.add_argument("--rows", type=int, default=30, help="tmux PTY rows")
    parser.add_argument("--startup-delay", type=float, default=4.0, help="seconds before trust capture")
    parser.add_argument("--timeout", type=float, default=30.0, help="seconds to wait for idle")
    parser.add_argument(
        "--working-delay",
        type=float,
        default=8.0,
        help="seconds to delay the mock tool-call stream for the working capture",
    )
    parser.add_argument("--keep-session", action="store_true", help="leave tmux session running")
    return parser.parse_args()


def require_executable(name: str) -> None:
    if shutil.which(name) is None:
        raise SystemExit(f"error: required executable not found: {name}")


def start_tmux_codebuddy(
    *,
    args: argparse.Namespace,
    command: str,
    session_name: str,
    home: Path,
    codebuddy_config: Path,
    xdg_config: Path,
    xdg_state: Path,
    base_url: str,
) -> str:
    shell_command = " ".join(
        [
            "cd",
            shlex.quote(str(args.cwd.resolve())),
            "&&",
            "env",
            f"HOME={shlex.quote(str(home.resolve()))}",
            f"CODEBUDDY_CONFIG_DIR={shlex.quote(str(codebuddy_config.resolve()))}",
            f"XDG_CONFIG_HOME={shlex.quote(str(xdg_config.resolve()))}",
            f"XDG_STATE_HOME={shlex.quote(str(xdg_state.resolve()))}",
            "CODEBUDDY_API_KEY=herdr-capture",
            f"CODEBUDDY_BASE_URL={shlex.quote(base_url)}",
            "NO_PROXY=127.0.0.1,localhost",
            "no_proxy=127.0.0.1,localhost",
            "HTTP_PROXY=",
            "HTTPS_PROXY=",
            "ALL_PROXY=",
            "http_proxy=",
            "https_proxy=",
            "all_proxy=",
            shlex.quote(command),
            ";",
            "printf",
            shlex.quote("\nEXIT:%s\n"),
            '"$?"',
            ";",
            "sleep",
            "30",
        ]
    )
    result = run(
        [
            "tmux",
            "new-session",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-s",
            session_name,
            "-x",
            str(args.cols),
            "-y",
            str(args.rows),
            shell_command,
        ]
    )
    return result.stdout.strip()


def wait_for_idle(pane_id: str, timeout: float) -> str:
    return wait_for_screen(
        pane_id,
        timeout,
        label="idle prompt",
        predicate=lambda screen: IDLE_PROMPT_RE.search(screen) is not None,
    )


def submit_prompt(pane_id: str, prompt: str) -> None:
    run(["tmux", "send-keys", "-l", "-t", pane_id, prompt])
    time.sleep(0.2)
    run(["tmux", "send-keys", "-t", pane_id, "Enter"])


def wait_for_permission(pane_id: str, timeout: float) -> str:
    return wait_for_screen(
        pane_id,
        timeout,
        label="permission prompt",
        predicate=lambda screen: PERMISSION_PROMPT in screen and "Bash command" in screen,
    )


def dismiss_permission_prompt(pane_id: str, timeout: float) -> str:
    run(["tmux", "send-keys", "-t", pane_id, "Escape"])
    try:
        return wait_for_idle(pane_id, min(timeout, 5.0))
    except RuntimeError:
        run(["tmux", "send-keys", "-t", pane_id, "Down"])
        run(["tmux", "send-keys", "-t", pane_id, "Down"])
        run(["tmux", "send-keys", "-t", pane_id, "Enter"])
        return wait_for_idle(pane_id, timeout)


def wait_for_working(pane_id: str, timeout: float) -> str:
    return wait_for_screen(
        pane_id,
        timeout,
        label="working indicator",
        predicate=lambda screen: WORKING_INDICATOR in screen.lower(),
    )


def wait_for_working_osc_title(pane_id: str, timeout: float) -> str:
    deadline = time.monotonic() + timeout
    last_title = ""
    while time.monotonic() < deadline:
        last_title = capture_pane_title(pane_id)
        if OSC_WORKING_TITLE_RE.search(last_title):
            return last_title
        time.sleep(0.2)
    return last_title


def wait_for_model_picker(pane_id: str, timeout: float) -> str:
    return wait_for_screen(
        pane_id,
        timeout,
        label="model picker",
        predicate=lambda screen: MODEL_PICKER_MARKER in screen
        and "Enter to confirm" in screen
        and "Esc to exit" in screen,
    )


def wait_for_model_picker_session(pane_id: str, timeout: float) -> str:
    return wait_for_screen(
        pane_id,
        timeout,
        label="session model picker",
        predicate=lambda screen: MODEL_PICKER_MARKER in screen
        and "Session" in screen
        and "Switch model for this session only" in screen,
    )


def wait_for_transcript_viewer(pane_id: str, timeout: float) -> str:
    return wait_for_screen(
        pane_id,
        timeout,
        label="transcript viewer",
        predicate=lambda screen: TRANSCRIPT_MARKER in screen,
    )


def wait_for_screen(
    pane_id: str,
    timeout: float,
    *,
    label: str,
    predicate: Callable[[str], bool],
) -> str:
    deadline = time.monotonic() + timeout
    last_screen = ""
    while time.monotonic() < deadline:
        last_screen = capture_pane_text(pane_id)
        if predicate(last_screen):
            return last_screen
        if "EXIT:" in last_screen:
            raise RuntimeError(f"CodeBuddy exited before {label}:\n{last_screen}")
        time.sleep(0.5)
    raise RuntimeError(f"timed out waiting for CodeBuddy {label}:\n{last_screen}")


def capture_pane(pane_id: str, output: Path, *, ansi: bool) -> None:
    command = ["tmux", "capture-pane", "-p", "-t", pane_id, "-S", "-200"]
    if ansi:
        command.insert(2, "-e")
    output.write_text(run(command).stdout, encoding="utf-8")


def capture_pane_text(pane_id: str) -> str:
    return run(["tmux", "capture-pane", "-p", "-t", pane_id, "-S", "-200"]).stdout


def capture_pane_title(pane_id: str) -> str:
    return run(["tmux", "display-message", "-p", "-t", pane_id, "#{pane_title}"]).stdout


def write_metadata(
    run_dir: Path,
    *,
    args: argparse.Namespace,
    command: str,
    base_url: str,
    session_name: str,
    pane_id: str,
) -> None:
    version = run([command, "--version"], check=False).stdout.strip()
    metadata = {
        "captured_at": iso_now(),
        "command": command,
        "codebuddy_version": version,
        "cwd": str(args.cwd.resolve()),
        "base_url": base_url,
        "session_name": session_name,
        "pane_id": pane_id,
        "cols": args.cols,
        "rows": args.rows,
        "fixture_files": list(FIXTURE_FILES),
        "env": {
            "CODEBUDDY_API_KEY": "herdr-capture",
            "CODEBUDDY_BASE_URL": base_url,
            "CODEBUDDY_CONFIG_DIR": str((run_dir / "codebuddy-config").resolve()),
        },
    }
    (run_dir / "metadata.json").write_text(
        json.dumps(metadata, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )


def write_requests(server: MockModelServer, output: Path) -> None:
    with output.open("w", encoding="utf-8") as fh:
        for request in server.requests:
            fh.write(json.dumps(request, sort_keys=True) + "\n")


def run(command: list[str], *, check: bool = True) -> subprocess.CompletedProcess[str]:
    completed = subprocess.run(command, capture_output=True, text=True, check=False)
    if check and completed.returncode != 0:
        raise RuntimeError(
            f"command failed ({completed.returncode}): {' '.join(command)}\n"
            f"stdout:\n{completed.stdout}\n"
            f"stderr:\n{completed.stderr}"
        )
    return completed


def parse_json_object(body: bytes) -> dict[str, Any]:
    try:
        value = json.loads(body.decode("utf-8")) if body else {}
    except (UnicodeDecodeError, json.JSONDecodeError):
        return {}
    return value if isinstance(value, dict) else {}


def iso_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except RuntimeError as err:
        print(f"error: {err}", file=sys.stderr)
        raise SystemExit(1)
