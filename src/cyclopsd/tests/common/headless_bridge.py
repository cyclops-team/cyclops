"""Two session-owned bridges beneath one fixture vendor, with explicit teardown."""

import json
import socket
import subprocess
import sys


def bridge(daemon_path, control_path, label):
    with socket.socket(socket.AF_UNIX) as control, socket.socket(socket.AF_UNIX) as daemon:
        control.connect(control_path)
        daemon.connect(daemon_path)
        commands = control.makefile("rwb", buffering=0)
        peer = daemon.makefile("rwb", buffering=0)
        json.loads(peer.readline())  # daemon greeting
        sequence = 0

        def request(method, params):
            nonlocal sequence
            sequence += 1
            peer.write((json.dumps({"id": sequence, "method": method, "params": params}) + "\n").encode())
            while True:
                response = json.loads(peer.readline())
                if response.get("id") == sequence:
                    return response

        registered = request("headless.bridge.register", {"label": label})
        commands.write((json.dumps(registered) + "\n").encode())
        for line in commands:
            command = json.loads(line)
            if command["method"] == "fixture.exit":
                commands.write(b'{"exiting":true}\n')
                return
            result = request(command["method"], command.get("params", {}))
            commands.write((json.dumps(result) + "\n").encode())


if sys.argv[1] == "--bridge":
    bridge(*sys.argv[2:])
else:
    daemon_path, first, second = sys.argv[1:]
    children = [
        subprocess.Popen(["python3", __file__, "--bridge", daemon_path, control, label])
        for control, label in [(first, "bridge-one"), (second, "bridge-two")]
    ]
    orphan = False
    try:
        for command in sys.stdin:
            orphan = command.strip() == "orphan"
            if orphan or command.strip() == "exit":
                break
    finally:
        if not orphan:
            for child in children:
                if child.poll() is None:
                    child.terminate()
            for child in children:
                child.wait()
