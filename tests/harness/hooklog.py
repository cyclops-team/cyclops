#!/usr/bin/env python3
"""Hook logger used by all CLI hook configs in the validation campaign.

Ported verbatim from the ADR-001 validation campaign
(cyclops-arch/validation/scripts/hooklog.py, 2026-08-01). Lives here so
harness probe tests can wire vendor hook configs at a stable in-repo path.

Usage as a hook command:  python3 hooklog.py /abs/path/log.ndjson
Reads the hook's JSON payload from stdin, appends one NDJSON line:
  {"ts_ns": ..., "event": <hook_event_name>, "prompt": <truncated>, "keys": [...], "data": {...}}
ts_ns is stamped as early as possible after interpreter start; python startup
overhead (~40-60 ms, measured separately) is a known constant of the method.
"""
import sys, time, json

ts = time.time_ns()
raw = sys.stdin.read()
try:
    d = json.loads(raw)
except Exception:
    d = {"raw": raw[:2000]}

interesting = ("message", "notification_type", "tool_name", "stop_hook_active",
               "reason", "trigger", "source", "turn_id", "type",
               "last-assistant-message", "session_id")
prompt = d.get("prompt")
if isinstance(prompt, str) and len(prompt) > 300:
    prompt = prompt[:300] + "..."

# --event <name> tags the event explicitly: agy hook payloads carry no
# event-name field, so a shared logger cannot tell events apart without it.
tag = None
if "--event" in sys.argv:
    tag = sys.argv[sys.argv.index("--event") + 1]

line = json.dumps({
    "ts_ns": ts,
    "event": tag or d.get("hook_event_name", d.get("type", "?")),
    "prompt": prompt,
    "keys": sorted(d.keys()),
    "data": {k: d[k] for k in interesting if k in d},
})
with open(sys.argv[1], "a") as f:
    f.write(line + "\n")
