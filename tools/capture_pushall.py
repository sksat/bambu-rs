#!/usr/bin/env python3
# /// script
# requires-python = ">=3.9"
# dependencies = []
# ///
"""Minimal clean-room MQTT-over-TLS client: connect, subscribe, send one
`pushall`, capture the report snapshot. Read-only — no control commands.

Uses only the Python stdlib (no third-party MQTT/Bambu code), so this counts as
direct device observation, not a reference implementation.
"""
import socket, ssl, os, json, sys, time

IP = os.environ["BAMBU_IP"]
SERIAL = os.environ["BAMBU_SERIAL"]
CODE = os.environ["BAMBU_CODE"]  # never printed
PORT = 8883
OUT = os.environ.get("BAMBU_OUT", "/tmp/pushall-raw.json")


def enc_len(n):
    out = bytearray()
    while True:
        b = n % 128
        n //= 128
        if n > 0:
            b |= 0x80
        out.append(b)
        if n == 0:
            break
    return bytes(out)


def enc_str(s):
    b = s.encode() if isinstance(s, str) else s
    return len(b).to_bytes(2, "big") + b


def connect_packet(client_id, user, pw):
    vh = enc_str("MQTT") + bytes([0x04, 0xC2]) + (60).to_bytes(2, "big")
    payload = enc_str(client_id) + enc_str(user) + enc_str(pw)
    return bytes([0x10]) + enc_len(len(vh) + len(payload)) + vh + payload


def subscribe_packet(pid, topic):
    vh = pid.to_bytes(2, "big")
    payload = enc_str(topic) + bytes([0x00])
    return bytes([0x82]) + enc_len(len(vh) + len(payload)) + vh + payload


def publish_packet(topic, payload):
    body = enc_str(topic) + payload
    return bytes([0x30]) + enc_len(len(body)) + body


ctx = ssl.create_default_context()
ctx.check_hostname = False
ctx.verify_mode = ssl.CERT_NONE

raw = socket.create_connection((IP, PORT), timeout=10)
s = ctx.wrap_socket(raw, server_hostname=IP)
s.settimeout(12)

s.sendall(connect_packet("bambu-rs-capture", "bblp", CODE))
hdr = s.recv(4)
if len(hdr) >= 4 and hdr[0] == 0x20:
    rc = hdr[3]
    if rc != 0:
        print(f"CONNACK rc={rc} (connect/auth failed)", file=sys.stderr)
        sys.exit(2)
else:
    print("unexpected CONNACK:", hdr.hex(), file=sys.stderr)
    sys.exit(2)

report_topic = f"device/{SERIAL}/report"
req_topic = f"device/{SERIAL}/request"
s.sendall(subscribe_packet(1, report_topic))
s.sendall(publish_packet(req_topic, json.dumps({"pushing": {"sequence_id": "0", "command": "pushall"}}).encode()))


def read_byte():
    b = s.recv(1)
    if not b:
        raise EOFError
    return b[0]


def read_remlen():
    mult, val = 1, 0
    while True:
        b = read_byte()
        val += (b & 0x7F) * mult
        if not (b & 0x80):
            break
        mult *= 128
    return val


def read_n(n):
    buf = bytearray()
    while len(buf) < n:
        chunk = s.recv(n - len(buf))
        if not chunk:
            raise EOFError
        buf += chunk
    return bytes(buf)


best = None
deadline = time.time() + 12
while time.time() < deadline:
    try:
        h = read_byte()
        rl = read_remlen()
        body = read_n(rl)
    except (socket.timeout, EOFError):
        break
    if (h & 0xF0) == 0x30:  # PUBLISH QoS0
        tlen = int.from_bytes(body[0:2], "big")
        payload = body[2 + tlen:]
        try:
            obj = json.loads(payload)
        except Exception:
            continue
        if best is None or len(payload) > best[0]:
            best = (len(payload), obj)

if not best:
    print("NO_REPORT_RECEIVED", file=sys.stderr)
    sys.exit(3)

with open(OUT, "w") as f:
    json.dump(best[1], f, indent=2, sort_keys=True)
print(f"OK wrote {best[0]} bytes to {OUT}")
print("TOP_LEVEL_KEYS:", sorted(best[1].keys()))
