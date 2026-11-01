"""Record what real `claude` does with an MCP image result, against loopback mocks only.

Starts the scripted Anthropic endpoint (`mock.py`, passed with --mock) with scenario
`call:{"name":"picture_tool","args":{}}` and `image_bridge.py` returning --image, then runs
`claude` with the driver's bridged argv, a fresh scratch HOME and working directory, and an env
of only HOME, PATH, ANTHROPIC_BASE_URL and a dummy ANTHROPIC_API_KEY. One task line goes in on
stdin, and stdin is closed once the `result` line is read, as the runtime does.

Writes <out>/12-bridge-image-result.{stdout.jsonl,stderr.txt,exit} with the scratch paths
redacted (/work, /home/operator, /scratch), then reads the mock's log and reports the request
`claude` sent its model after the tool call: the `tool_result` block for toolu_1, with each
base64 cut short, and whether it carries the bridge's image bytes.

Usage: python3 -I record_image_mock.py --mock <mock.py> --image <png> --out <dir>
                                        [--claude claude] [--name 12-bridge-image-result]
Standard library only. Never uses the real HOME.
"""
import argparse
import base64
import binascii
import hashlib
import json
import os
import re
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time
import uuid

HERE = os.path.dirname(os.path.abspath(__file__))
TASK = "Call picture_tool once, then say what the picture shows."
SYSTEM_PROMPT = "You are a Murmur capsule."
SERVER = "claude_bridge"


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def wait_listening(port, proc, what):
    for _ in range(100):
        if proc.poll() is not None:
            sys.exit(f"{what} exited with {proc.returncode} before listening")
        try:
            socket.create_connection(("127.0.0.1", port), 0.1).close()
            return
        except OSError:
            time.sleep(0.05)
    sys.exit(f"{what} never listened on {port}")


def short(data):
    if isinstance(data, str) and len(data) > 40:
        return f"{data[:16]}…{data[-16:]} ({len(data)} chars)"
    return data


def abbreviate(value):
    """The value with every long string under a `data` key cut to its ends and length."""
    if isinstance(value, dict):
        return {k: short(v) if k == "data" else abbreviate(v) for k, v in value.items()}
    if isinstance(value, list):
        return [abbreviate(v) for v in value]
    return value


def png_size(raw):
    if raw[:8] == b"\x89PNG\r\n\x1a\n" and raw[12:16] == b"IHDR":
        return struct.unpack(">II", raw[16:24])
    return None


def image_blocks(value):
    if isinstance(value, dict):
        if value.get("type") == "image":
            yield value
        for v in value.values():
            yield from image_blocks(v)
    elif isinstance(value, list):
        for v in value:
            yield from image_blocks(v)


def tool_result_for(body, tool_use_id):
    for message in body.get("messages", []):
        content = message.get("content")
        if not isinstance(content, list):
            continue
        for block in content:
            if block.get("type") == "tool_result" and block.get("tool_use_id") == tool_use_id:
                return block
    return None


def is_result(raw):
    try:
        return json.loads(raw).get("type") == "result"
    except ValueError:
        return False


def redact(text, work, home, scratch):
    # `claude` names a project directory after its path with every non-alphanumeric as `-`.
    slug = lambda path: re.sub(r"[^A-Za-z0-9]", "-", path)
    for real, shown in [(work, "/work"), (slug(work), "-work"), (home, "/home/operator"),
                        (scratch, "/scratch")]:
        text = text.replace(real, shown)
    return text


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--mock", required=True)
    parser.add_argument("--image", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--claude", default="claude")
    parser.add_argument("--name", default="12-bridge-image-result")
    args = parser.parse_args()

    claude = shutil.which(args.claude) or sys.exit(f"no {args.claude} on PATH")
    version = subprocess.run([claude, "--version"], capture_output=True, text=True).stdout.strip()
    scratch = tempfile.mkdtemp(prefix="cc-image-mock.")
    home, work, mock_log = (os.path.join(scratch, d) for d in ("home", "work", "mocklog"))
    for d in (home, work, mock_log):
        os.makedirs(d)
    bridge_log = os.path.join(scratch, "bridge-log.jsonl")
    scen = os.path.join(scratch, "scenario")
    with open(scen, "w") as f:
        f.write('call:{"name":"picture_tool","args":{}}')
    mock_port, bridge_port = free_port(), free_port()
    mcp_config = os.path.join(scratch, "mcp-config.json")
    with open(mcp_config, "w") as f:
        json.dump({"mcpServers": {SERVER: {"type": "http", "url": f"http://127.0.0.1:{bridge_port}/mcp",
                                            "headers": {"Authorization": "Bearer mock-bridge-token"}}}}, f)

    servers = [
        subprocess.Popen([sys.executable, "-I", os.path.abspath(args.mock), str(mock_port)],
                         env={"MOCK_LOG": mock_log, "MOCK_SCEN": scen, "PATH": os.environ["PATH"]}),
        subprocess.Popen([sys.executable, "-I", os.path.join(HERE, "image_bridge.py"), str(bridge_port)],
                         env={"BRIDGE_LOG": bridge_log, "BRIDGE_IMAGE": os.path.abspath(args.image),
                              "PATH": os.environ["PATH"]}),
    ]
    try:
        wait_listening(mock_port, servers[0], "mock.py")
        wait_listening(bridge_port, servers[1], "image_bridge.py")
        session = str(uuid.uuid4())
        argv = [claude, "--print", "--output-format", "stream-json", "--verbose",
                "--input-format", "stream-json", "--include-partial-messages", "--setting-sources", "",
                "--session-id", session, "--tools", f"mcp__{SERVER}__picture_tool",
                "--mcp-config", mcp_config, "--strict-mcp-config",
                "--permission-mode", "bypassPermissions", "--system-prompt", SYSTEM_PROMPT]
        env = {"HOME": home, "PATH": os.environ["PATH"],
               "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{mock_port}",
               "ANTHROPIC_API_KEY": "sk-ant-mock-not-a-key"}
        proc = subprocess.Popen(argv, cwd=work, env=env, stdin=subprocess.PIPE,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        line = {"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": TASK}]}}
        proc.stdin.write((json.dumps(line) + "\n").encode())
        proc.stdin.flush()
        stdout = []
        for raw in proc.stdout:
            stdout.append(raw)
            # 2.1.316 prints the `result` line's keys in another order, so read its type.
            if is_result(raw) and not proc.stdin.closed:
                proc.stdin.close()
        stderr = proc.stderr.read()
        code = proc.wait(timeout=60)
    finally:
        for s in servers:
            s.terminate()

    os.makedirs(args.out, exist_ok=True)
    base = os.path.join(args.out, args.name)
    with open(base + ".stdout.jsonl", "w") as f:
        f.write(redact(b"".join(stdout).decode(), work, home, scratch))
    with open(base + ".stderr.txt", "w") as f:
        f.write(redact(stderr.decode(), work, home, scratch))
    with open(base + ".exit", "w") as f:
        f.write(f"{code}\n")

    with open(args.image, "rb") as f:
        image = f.read()
    print(f"claude: {version}")
    print(f"session: {session}")
    print(f"scratch (kept): {scratch}")
    print(f"exit: {code}; stdout lines: {len(stdout)}; stderr bytes: {len(stderr)}")
    print(f"bridge image: {len(image)} bytes, sha256 {hashlib.sha256(image).hexdigest()}, "
          f"png {png_size(image)}")

    requests = []
    for name in sorted(os.listdir(mock_log)):
        with open(os.path.join(mock_log, name)) as f:
            entry = json.load(f)
        if "/v1/messages" in entry["path"] and "count_tokens" not in entry["path"]:
            requests.append((name, entry))
    print(f"/v1/messages requests: {[n for n, _ in requests]}")
    after = [(n, e) for n, e in requests if '"tool_result"' in e["body"]]
    if not after:
        print("no request carried a tool_result")
        return
    name, entry = after[0]
    block = tool_result_for(json.loads(entry["body"]), "toolu_1")
    print(f"request after the tool call: {name} ({len(entry['body'])} body bytes)")
    print("tool_result block for toolu_1:")
    print(json.dumps(abbreviate(block), indent=2))
    images = list(image_blocks(block))
    if not images:
        print("CLASSIFICATION: (c) no image block in the tool_result")
    for img in images:
        source = img.get("source") if isinstance(img.get("source"), dict) else {}
        data = source.get("data", img.get("data"))
        decoded = base64.b64decode(data) if isinstance(data, str) else b""
        digest = hashlib.sha256(decoded).hexdigest()
        same = decoded == image
        print(f"image block: media_type {source.get('media_type', img.get('mimeType'))}, "
              f"{len(decoded)} decoded bytes, sha256 {digest}, png {png_size(decoded)}, "
              f"base64 identical to the bridge's: {data == base64.b64encode(image).decode()}")
        print("CLASSIFICATION: " + ("(a) image block with the bridge's exact bytes" if same
                                    else "(b) image block, re-encoded"))

    for raw in stdout:
        value = json.loads(raw)
        if value.get("type") != "user":
            continue
        for img in image_blocks(value):
            data = (img.get("source") or {}).get("data") if isinstance(img.get("source"), dict) else img.get("data")
            shape = "anthropic (source.media_type/source.data)" if isinstance(img.get("source"), dict) \
                else "mcp (mimeType/data)"
            print(f"stdout user line image shape: {shape}; carries base64: {isinstance(data, str) and len(data) > 0}"
                  f"; base64 identical to the bridge's: {data == base64.b64encode(image).decode()}")
            print("stdout user line image block: " + json.dumps(abbreviate(img)))


if __name__ == "__main__":
    main()
