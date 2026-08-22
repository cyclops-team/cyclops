#!/usr/bin/env python3
"""Hold an authenticated socket until the topology test releases it."""

import json
import os
import socket
import sys
import time

WAIT = 10.0


def main() -> None:
    socket_path, result_file, ready_file, send_file, params_json = sys.argv[1:6]
    client = socket.socket(socket.AF_UNIX)
    client.settimeout(WAIT)
    client.connect(socket_path)
    stream = client.makefile("rwb")
    stream.readline()

    with open(ready_file, "w") as ready:
        ready.write(str(os.getpid()))

    deadline = time.monotonic() + WAIT
    while not os.path.exists(send_file):
        if time.monotonic() >= deadline:
            raise TimeoutError(f"timed out waiting for {send_file}")
        time.sleep(0.02)

    request = {
        "id": 1,
        "method": "agent.state.report",
        "params": json.loads(params_json),
    }
    stream.write((json.dumps(request) + "\n").encode())
    stream.flush()

    for raw in stream:
        response = json.loads(raw)
        if response.get("id") == 1:
            with open(result_file, "w") as result:
                result.write(json.dumps(response))
            return
    raise RuntimeError("daemon closed the socket before replying")


if __name__ == "__main__":
    main()
