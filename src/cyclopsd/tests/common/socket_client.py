"""One msg.send over the daemon's Unix socket, from THIS process.

Exists so a test can choose the process the request comes from. The
daemon resolves the sender from the calling process's ancestry, so who
runs this script is the whole point: inside a watched pane it must
resolve to that pane, and under a vendor process outside every watched
pane it must be refused.

Writes the response line to a file rather than stdout: the in-pane case
runs under tmux, where stdout is the terminal.
"""

import json, socket, sys

sock_path, out_path, method, params = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
s = socket.socket(socket.AF_UNIX)
s.connect(sock_path)
f = s.makefile("rwb")
f.readline()  # hello
req = {"id": 1, "method": method, "params": json.loads(params)}
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
        break
