#!/usr/bin/env python3
"""Drive a QEMU serial console (unix socket) from scripts.

    tools/dev/serial.py SOCK expect 'login:' [TIMEOUT]      wait for text, print what arrived
    tools/dev/serial.py SOCK send 'root\\n'                   send text (escapes decoded)
    tools/dev/serial.py SOCK run 'cmd' [TIMEOUT]              send cmd, wait for the prompt marker
    tools/dev/serial.py SOCK tail [SECONDS]                   dump whatever arrives for a while

`run` expects a shell whose prompt was set to `JOS> ` (see `login`), which
is what tools/dev/freebsd-vm.sh's first-boot script does; it returns the
command's output and exits with the command's status (read back with
`echo $?`).

    tools/dev/serial.py SOCK login USER PASSWORD [TIMEOUT]    log in on the console and set the prompt
"""

import socket
import sys
import time

PROMPT = b"JOS> "


class Serial:
    def __init__(self, path):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.connect(path)
        self.s.settimeout(0.2)
        self.buf = b""

    def read_some(self):
        try:
            data = self.s.recv(65536)
        except socket.timeout:
            return b""
        if not data:
            raise EOFError("serial socket closed")
        self.buf += data
        return data

    def expect(self, needle, timeout):
        """Wait until `needle` shows up; returns everything up to and including it."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            idx = self.buf.find(needle)
            if idx >= 0:
                out, self.buf = self.buf[: idx + len(needle)], self.buf[idx + len(needle) :]
                return out
            self.read_some()
        raise TimeoutError(f"timed out waiting for {needle!r}; last output:\n{self.buf[-2000:].decode(errors='replace')}")

    def send(self, text):
        self.s.sendall(text.encode())

    def drain(self, seconds):
        deadline = time.monotonic() + seconds
        out = b""
        while time.monotonic() < deadline:
            out += self.read_some()
        self.buf = b""
        return out


def decode(s):
    return bytes(s, "utf-8").decode("unicode_escape")


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        sys.exit(2)
    ser = Serial(sys.argv[1])
    cmd = sys.argv[2]
    if cmd == "expect":
        timeout = float(sys.argv[4]) if len(sys.argv) > 4 else 60
        out = ser.expect(decode(sys.argv[3]).encode(), timeout)
        sys.stdout.write(out.decode(errors="replace"))
    elif cmd == "send":
        ser.send(decode(sys.argv[3]))
        # QEMU drops input still queued when the client hangs up: linger.
        ser.drain(0.5)
    elif cmd == "tail":
        secs = float(sys.argv[3]) if len(sys.argv) > 3 else 5
        sys.stdout.write(ser.drain(secs).decode(errors="replace"))
    elif cmd == "run":
        timeout = float(sys.argv[4]) if len(sys.argv) > 4 else 600
        ser.drain(0.3)
        ser.send(sys.argv[3] + "\n")
        out = ser.expect(PROMPT, timeout)
        text = out.decode(errors="replace")
        # Drop the echoed command line and the prompt.
        lines = text.split("\n")
        if lines and sys.argv[3].strip() in lines[0]:
            lines = lines[1:]
        if lines and lines[-1].strip() == PROMPT.decode().strip():
            lines = lines[:-1]
        sys.stdout.write("\n".join(lines))
    elif cmd == "login":
        user, password = sys.argv[3], sys.argv[4]
        timeout = float(sys.argv[5]) if len(sys.argv) > 5 else 300
        ser.send("\n")
        ser.expect(b"login:", timeout)
        ser.send(user + "\n")
        if password:
            ser.expect(b"assword:", 30)
            ser.send(password + "\n")
        # login(1) flushes the tty before starting the shell, so keep setting
        # the prompt until it answers. The assignment is split so that the
        # echoed command line does not contain the marker itself.
        for _ in range(20):
            time.sleep(1.5)
            ser.send('PS1="JOS""> "; export PS1\n')
            try:
                ser.expect(PROMPT, 3)
                break
            except TimeoutError:
                continue
        else:
            raise TimeoutError("shell did not answer")
        ser.drain(0.5)
        print("logged in")
    else:
        print(__doc__)
        sys.exit(2)


if __name__ == "__main__":
    try:
        main()
    except (TimeoutError, EOFError) as e:
        print(f"serial: {e}", file=sys.stderr)
        sys.exit(1)
