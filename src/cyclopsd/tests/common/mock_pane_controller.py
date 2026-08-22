#!/usr/bin/env python3
"""Keep the pane root alive while tests replace its agent child."""

import os
import signal
import sys


def write(path: str, value: object) -> None:
    with open(path, "w") as output:
        output.write(str(value))


def give_terminal_to(pgid: int) -> None:
    try:
        tty = os.open("/dev/tty", os.O_RDWR)
        os.tcsetpgrp(tty, pgid)
        os.close(tty)
    except OSError:
        pass


def main() -> None:
    if len(sys.argv) != 4:
        sys.exit("usage: mock_pane_controller.py FIFO AGENT_BINARY PID_FILE")

    fifo, agent_binary, pid_file = sys.argv[1:]
    controller_pgid = os.getpgrp()
    signal.signal(signal.SIGTTOU, signal.SIG_IGN)
    write(pid_file, os.getpid())
    agent_pid: int | None = None

    with open(fifo) as commands:
        for raw in commands:
            command, *args = raw.rstrip("\n").split("\t")
            if command == "start":
                agent_fifo, agent_pid_file = args
                pid = os.fork()
                if pid == 0:
                    os.setpgrp()
                    os.execv(agent_binary, ["cycauth-agent", agent_fifo])
                agent_pid = pid
                try:
                    os.setpgid(pid, pid)
                except OSError:
                    pass
                give_terminal_to(pid)
                write(agent_pid_file, pid)
            elif command == "wait":
                if agent_pid is not None:
                    try:
                        os.waitpid(agent_pid, 0)
                    except ChildProcessError:
                        pass
                give_terminal_to(controller_pgid)
                write(args[0], "done")
            elif command == "quit":
                break
            else:
                raise ValueError(f"unknown controller command: {command}")

    if agent_pid is not None:
        try:
            os.kill(agent_pid, signal.SIGTERM)
            os.waitpid(agent_pid, 0)
        except (ProcessLookupError, ChildProcessError):
            pass


if __name__ == "__main__":
    main()
