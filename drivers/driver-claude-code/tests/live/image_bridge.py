"""A minimal MCP streamable-HTTP server that answers like murmur's claude_bridge with an image.

One tool, `picture_tool`. Its `tools/call` answer has the shape murmur's bridge gives a bridged
call whose tool returned one attachment (murmur `claude_bridge.rs` `tool_result()`, since
4f65bb98): a text item carrying the tool's output and the `[image attached: …]` line, then an
MCP `image` item with the file's bytes as standard padded base64.

Env: BRIDGE_IMAGE (the PNG to return, required), BRIDGE_LOG (request log, JSON lines).
Usage: BRIDGE_IMAGE=<png> BRIDGE_LOG=<file> python3 -I image_bridge.py <port>
Standard library only.
"""
import base64
import json
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOG = os.environ.get("BRIDGE_LOG", "/tmp/image-bridge-log.jsonl")
IMAGE = os.environ["BRIDGE_IMAGE"]
TOOLS = [{"name": "picture_tool", "description": "Returns a picture.",
          "inputSchema": {"type": "object", "properties": {}}}]


def picture_result():
    with open(IMAGE, "rb") as f:
        data = f.read()
    text = f"PICTURE-TOOL-RAN\n[image attached: picture.png, image/png, {len(data)} bytes]"
    return {"content": [{"type": "text", "text": text},
                        {"type": "image", "data": base64.b64encode(data).decode("ascii"),
                         "mimeType": "image/png"}],
            "isError": False}


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def do_GET(self):
        self.send_response(405); self.send_header("content-length", "0"); self.end_headers()

    def do_DELETE(self):
        self.send_response(200); self.send_header("content-length", "0"); self.end_headers()

    def do_POST(self):
        n = int(self.headers.get("content-length") or 0)
        msg = json.loads(self.rfile.read(n) or b"{}")
        with open(LOG, "a") as f:
            f.write(json.dumps({"auth": self.headers.get("authorization"), "msg": msg}) + "\n")
        if "id" not in msg:
            self.send_response(202); self.send_header("content-length", "0"); self.end_headers(); return
        m = msg.get("method")
        if m == "initialize":
            res = {"protocolVersion": msg["params"].get("protocolVersion", "2025-06-18"),
                   "capabilities": {"tools": {}}, "serverInfo": {"name": "image-bridge", "version": "0"}}
        elif m == "tools/list":
            res = {"tools": TOOLS}
        elif m == "tools/call":
            res = picture_result()
        else:
            res = {}
        b = json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": res}).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("mcp-session-id", "s1")
        self.send_header("content-length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
