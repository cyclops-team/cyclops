"""Two requests on one connection, with an exec in between.

The daemon attests who opened a connection and asks again at every
request that turns the connection into authority. This is the client that
makes the second answer differ from the first: it sends once, replaces
itself with another program while keeping the same socket, and sends
again on that same connection.

The second request must be refused. The pid did not change, so nothing
but the kernel's own execution generation can tell the two apart.

Phase 1: connect, send, then exec into phase 2 with the socket's
descriptor inherited. Phase 2: rebuild the socket from that descriptor
and send again. Each phase writes its reply to its own file.
"""

import json
import os
import socket
import sys


def send(sock, out_path, subject):
    f = sock.makefile("rwb")
    req = {
        "id": 1,
        "method": "msg.send",
        "params": {"to": ["hooky"], "subject": subject, "body": "b"},
    }
    f.write((json.dumps(req) + "\n").encode())
    f.flush()
    while True:
        line = f.readline()
        if not line:
            break
        v = json.loads(line)
        # Events share the stream; the reply is the line carrying our id.
        if v.get("id") == 1:
            open(out_path, "w").write(json.dumps(v))
            return
    open(out_path, "w").write(json.dumps({"error": {"code": "no_reply"}}))


if sys.argv[1] == "phase2":
    fd, out_path = int(sys.argv[2]), sys.argv[3]
    send(socket.socket(fileno=fd), out_path, "after-exec")
    sys.exit(0)

sock_path, first_out, second_out = sys.argv[1], sys.argv[2], sys.argv[3]
s = socket.socket(socket.AF_UNIX)
s.connect(sock_path)
s.makefile("rb").readline()  # hello
send(s, first_out, "before-exec")

# The descriptor has to survive the exec; Python marks sockets
# close-on-exec by default.
fd = s.fileno()
os.set_inheritable(fd, True)
os.execv(sys.executable, [sys.executable, __file__, "phase2", str(fd), second_out])
