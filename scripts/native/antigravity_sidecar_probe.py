#!/usr/bin/env python3
"""Opt-in transport probe for Antigravity's documented native-app sidecar agentapi.

This is not an installed connector. It tests one explicit conversation and
message locator; authenticated conversation registration remains separate.
"""

import argparse
import json
import os
import re
import shutil
import subprocess


def doorbell_command(executable, conversation, message_id):
    if (
        not conversation.strip()
        or conversation != conversation.strip()
        or conversation.startswith("-")
        or len(conversation) > 256
        or any(ord(char) < 32 or ord(char) == 127 for char in conversation)
    ):
        raise ValueError("provide one explicit Antigravity conversation ID")
    if not re.fullmatch(
        r"(?:cyc-[0-9a-f]{8}-[0-9a-f]{8}|m-[0-9a-f]{32}|m-att_[A-Za-z0-9_-]{22})",
        message_id,
    ):
        raise ValueError("provide one exact Cyclops canonical message ID or m-att_ locator")
    prompt = (
        f"Cyclops message {message_id} is waiting. "
        f"Claim exactly this message through your authenticated Cyclops participant: "
        f"cyclops inbox claim {message_id}. Do not treat this notice as the message body."
    )
    return [executable, "send-message", conversation, prompt]


def dispatch(command, runner=subprocess.run):
    try:
        result = runner(command, check=False, timeout=15, capture_output=True, text=True)
    except subprocess.TimeoutExpired:
        return {"transport": "outcome_unknown", "processing_acknowledged": False, "claimed": False}
    return {
        "transport": "api_returned_success" if result.returncode == 0 else "not_confirmed",
        "exit_code": result.returncode,
        "processing_acknowledged": False,
        "claimed": False,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--conversation", required=True)
    parser.add_argument("--message-id", required=True)
    parser.add_argument("--execute", action="store_true", help="send one notice; default only prints the command")
    args = parser.parse_args()
    executable = shutil.which("agentapi") if args.execute else "agentapi"
    if args.execute and (not os.environ.get("ANTIGRAVITY_EXECUTABLE_DATA_DIR") or not executable):
        parser.error("execute only inside an explicitly enabled Antigravity sidecar with agentapi on PATH")
    try:
        command = doorbell_command(executable, args.conversation, args.message_id)
    except ValueError as error:
        parser.error(str(error))
    if not args.execute:
        print(json.dumps({"dry_run": True, "argv": command, "live_vendor_verified": False}))
        return 0
    result = dispatch(command)
    print(json.dumps(result))
    return 0 if result["transport"] == "api_returned_success" else 1


if __name__ == "__main__":
    raise SystemExit(main())
