"""Runs `grok` with exactly the argv and env-set a plan written by `examples/write_plan.rs` holds.

The environment starts empty, as murmur starts a harness's, then gets HOME and PATH (what
`capabilities.env.allow: [HOME, PATH]` passes), the mock's XAI_API_KEY and GROK_XAI_API_BASE_URL
(what a mock run needs; a real capsule never allows either), and the plan's env-set last.

Usage: python3 -I run_plan.py <plan.json> <grok binary> <scratch HOME> <PATH> <mock base URL> \
           <work dir> <out prefix>
Writes <out prefix>.stdout.jsonl, .stderr.txt and .exit. Refuses a HOME outside /space/scratch/,
because a stored grok.com login outranks XAI_API_KEY and would spend the subscription.
Standard library only.
"""
import json
import subprocess
import sys

plan_path, grok, home, path, base_url, work, out = sys.argv[1:8]
if not home.startswith("/space/scratch/"):
    sys.exit(f"refusing HOME {home}: use a scratch HOME with no auth.json")
plan = json.load(open(plan_path))
env = {"HOME": home, "PATH": path, "XAI_API_KEY": "xai-test", "GROK_XAI_API_BASE_URL": base_url}
env.update({name: value for name, value in plan["env_set"]})
with open(out + ".stdout.jsonl", "wb") as stdout, open(out + ".stderr.txt", "wb") as stderr:
    code = subprocess.run([grok, *plan["args"]], cwd=work, env=env, stdin=subprocess.DEVNULL,
                          stdout=stdout, stderr=stderr, timeout=120).returncode
with open(out + ".exit", "w") as f:
    f.write(f"{code}\n")
print("exit", code)
