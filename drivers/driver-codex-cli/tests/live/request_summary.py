"""Summarises a request body the mock captured: the model-visible tools (with namespaces), the
instructions, every input message's role and text, and the other top-level fields. Base64 image
data is replaced by its length and sha256.

Usage: python3 -I request_summary.py <body.json> [--full]
"""
import base64
import hashlib
import json
import re
import sys

body = json.load(open(sys.argv[1]))
full = "--full" in sys.argv[2:]
cut = (lambda s: s) if full else (lambda s: s if len(s) <= 400 else s[:400] + f"…(+{len(s) - 400})")


def scrub(text):
    def image(m):
        data = m.group(2)
        try:
            digest = hashlib.sha256(base64.b64decode(data)).hexdigest()
        except Exception:
            digest = "undecodable"
        return f"{m.group(1)}<{len(data)} base64 chars, sha256 {digest}>"
    return re.sub(r"(data:image/[a-z]+;base64,)([A-Za-z0-9+/=]+)", image, text)


def names(tools, prefix=""):
    out = []
    for t in tools or []:
        if t.get("type") == "namespace":
            out += names(t.get("tools"), t.get("name", "") + "/")
        else:
            out.append(prefix + (t.get("name") or t.get("type")))
    return out


print("TOOLS", ", ".join(names(body.get("tools"))))
print("INSTRUCTIONS", cut(json.dumps(body.get("instructions"))))
for item in body.get("input", []):
    kind, role = item.get("type"), item.get("role")
    content = item.get("content")
    if isinstance(content, list):
        for part in content:
            print("INPUT", kind, role, cut(scrub(json.dumps(part.get("text", part)))))
    else:
        print("INPUT", kind, role, cut(scrub(json.dumps(item))))
for key in sorted(body):
    if key not in ("tools", "instructions", "input"):
        print("FIELD", key, cut(json.dumps(body[key])))
