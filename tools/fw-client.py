#!/usr/bin/env python3
"""A fake downstream qube for `tools/uefi-test.sh firewall`.

Connects to QEMU's `-netdev socket,listen=:PORT` (4-byte big-endian length
prefix per Ethernet frame) as the client declared in the firewall config
(MAC 02:4a:5b:98:d2:60, expected IP 10.137.0.3) and checks, in order:

1. DHCP DISCOVER -> OFFER from the firewall with the configured IP;
2. ARP for the gateway -> reply with the downstream NIC's MAC;
3. ICMP echo to UPSTREAM (QEMU's user-net gateway 10.0.2.2) -> reply,
   which proves rule evaluation, NAT out, conntrack and NAT back.

Exit status 0 when all three pass. Pure standard library.
"""

import os
import socket
import struct
import sys
import time

PORT = int(os.environ.get("FW_PORT", "7777"))
MAC = bytes.fromhex("024a5b98d260")
WANT_IP = "10.137.0.3"
GATEWAY = "10.137.0.2"
UPSTREAM = os.environ.get("FW_UPSTREAM", "10.0.2.2")
BCAST = b"\xff" * 6
TIMEOUT = float(os.environ.get("FW_TIMEOUT", "20"))


def csum(data: bytes) -> int:
    if len(data) % 2:
        data += b"\0"
    s = sum(struct.unpack("!%dH" % (len(data) // 2), data))
    while s >> 16:
        s = (s & 0xFFFF) + (s >> 16)
    return (~s) & 0xFFFF


def ipv4(src, dst, proto, payload):
    hdr = struct.pack("!BBHHHBBH4s4s", 0x45, 0, 20 + len(payload), 0x1234, 0, 64, proto, 0,
                      socket.inet_aton(src), socket.inet_aton(dst))
    hdr = hdr[:10] + struct.pack("!H", csum(hdr)) + hdr[12:]
    return hdr + payload


def udp(src, dst, sport, dport, payload):
    length = 8 + len(payload)
    return ipv4(src, dst, 17, struct.pack("!HHHH", sport, dport, length, 0) + payload)


def eth(dst, src, ethertype, payload):
    return dst + src + struct.pack("!H", ethertype) + payload


def dhcp_discover(xid):
    msg = struct.pack("!BBBBIHH4s4s4s4s", 1, 1, 6, 0, xid, 0, 0x8000,
                      b"\0" * 4, b"\0" * 4, b"\0" * 4, b"\0" * 4)
    msg += MAC + b"\0" * 10 + b"\0" * 192 + b"\x63\x82\x53\x63"
    msg += bytes([53, 1, 1]) + bytes([55, 2, 1, 3]) + b"\xff"
    return eth(BCAST, MAC, 0x0800, udp("0.0.0.0", "255.255.255.255", 68, 67, msg))


def arp_request(target_ip):
    body = struct.pack("!HHBBH", 1, 0x0800, 6, 4, 1) + MAC + socket.inet_aton(WANT_IP) \
        + b"\0" * 6 + socket.inet_aton(target_ip)
    return eth(BCAST, MAC, 0x0806, body)


def icmp_echo(gw_mac, dst, ident, seq):
    payload = struct.pack("!BBHHH", 8, 0, 0, ident, seq) + b"juanitos"
    payload = payload[:2] + struct.pack("!H", csum(payload)) + payload[4:]
    return eth(gw_mac, MAC, 0x0800, ipv4(WANT_IP, dst, 1, payload))


class Link:
    def __init__(self):
        self.sock = socket.create_connection(("127.0.0.1", PORT), timeout=TIMEOUT)
        self.buf = b""

    def send(self, frame):
        self.sock.sendall(struct.pack("!I", len(frame)) + frame)

    def recv(self, deadline):
        while True:
            if len(self.buf) >= 4:
                n = struct.unpack("!I", self.buf[:4])[0]
                if len(self.buf) >= 4 + n:
                    frame = self.buf[4:4 + n]
                    self.buf = self.buf[4 + n:]
                    return frame
            left = deadline - time.monotonic()
            if left <= 0:
                return None
            self.sock.settimeout(left)
            try:
                chunk = self.sock.recv(65536)
            except socket.timeout:
                return None
            if not chunk:
                return None
            self.buf += chunk


def wait_for(link, pred, what, resend=None, every=1.0):
    deadline = time.monotonic() + TIMEOUT
    last = 0.0
    while True:
        if resend and time.monotonic() - last >= every:
            link.send(resend)
            last = time.monotonic()
        frame = link.recv(min(deadline, time.monotonic() + every))
        if frame is None:
            if time.monotonic() >= deadline:
                print(f"FAIL: no {what} within {TIMEOUT}s")
                sys.exit(1)
            continue
        r = pred(frame)
        if r is not None:
            return r


def main():
    link = Link()
    print(f"connected to QEMU socket netdev on port {PORT}")

    xid = 0x4A05
    def is_offer(f):
        if len(f) < 14 + 20 + 8 + 240 or f[12:14] != b"\x08\x00" or f[23] != 17:
            return None
        ip = f[14:34]
        dh = f[14 + 20 + 8:]
        if dh[0] != 2 or struct.unpack("!I", dh[4:8])[0] != xid:
            return None
        return socket.inet_ntoa(dh[16:20]), socket.inet_ntoa(ip[12:16])
    yiaddr, server = wait_for(link, is_offer, "DHCP OFFER", resend=dhcp_discover(xid))
    print(f"DHCP OFFER: {yiaddr} from {server}")
    if yiaddr != WANT_IP or server != GATEWAY:
        print(f"FAIL: expected {WANT_IP} from {GATEWAY}")
        sys.exit(1)

    def is_arp_reply(f):
        if f[12:14] != b"\x08\x06" or struct.unpack("!H", f[20:22])[0] != 2:
            return None
        if socket.inet_ntoa(f[28:32]) != GATEWAY:
            return None
        return f[22:28]
    gw_mac = wait_for(link, is_arp_reply, "ARP reply", resend=arp_request(GATEWAY))
    print(f"ARP: {GATEWAY} is at {gw_mac.hex(':')}")

    def echo_reply_from(expect_src, ident, seq):
        def pred(f):
            if len(f) < 14 + 20 + 8 or f[12:14] != b"\x08\x00" or f[23] != 1:
                return None
            ip = f[14:34]
            ic = f[34:42]
            if ic[0] != 0 or struct.unpack("!HH", ic[4:8]) != (ident, seq):
                return None
            src, dst = socket.inet_ntoa(ip[12:16]), socket.inet_ntoa(ip[16:20])
            return (src, dst) if src == expect_src else None
        return pred

    # 1. The gateway answers pings itself: downstream path only.
    src, dst = wait_for(link, echo_reply_from(GATEWAY, 0x4A05, 1), "echo reply from the gateway",
                        resend=icmp_echo(gw_mac, GATEWAY, 0x4A05, 1))
    print(f"ICMP echo reply from the gateway: {src} -> {dst}")

    # 2. Through NAT to the upstream network and back.
    src, dst = wait_for(link, echo_reply_from(UPSTREAM, 0x4A06, 1), "echo reply via NAT",
                        resend=icmp_echo(gw_mac, UPSTREAM, 0x4A06, 1))
    print(f"ICMP echo reply via NAT: {src} -> {dst}")
    if dst != WANT_IP:
        print("FAIL: reply not translated back to the client address")
        sys.exit(1)
    print("OK: DHCP, ARP, gateway ping and NAT forwarding all work")


if __name__ == "__main__":
    main()
