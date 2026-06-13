#!/usr/bin/env python3
# /// script
# requires-python = ">=3.9"
# dependencies = []
# ///
"""Capture the pushall snapshot AND every subsequent report delta over a window.

Read-only — sends one `pushall`, then just records what the printer pushes. Use
it to OBSERVE the real wire format of deltas (e.g. partial `ams.ams[].tray[]`
updates) before modelling them: run this, then trigger the change you want to
see (load/unload filament, change a setting) in Bambu Handy/Studio, and inspect
the captured JSONL.

Stdlib only (no third-party MQTT/Bambu code), so this is direct device
observation, not a reference implementation. The access code is never printed.

Env: BAMBU_IP, BAMBU_SERIAL, BAMBU_CODE; optional BAMBU_OUT (default
/tmp/report-delta.jsonl), BAMBU_WINDOW seconds (default 60).
"""
import socket, ssl, os, json, sys, time

IP = os.environ["BAMBU_IP"]
SERIAL = os.environ["BAMBU_SERIAL"]
CODE = os.environ["BAMBU_CODE"]  # never printed
PORT = 8883
OUT = os.environ.get("BAMBU_OUT", "/tmp/report-delta.jsonl")
WINDOW = float(os.environ.get("BAMBU_WINDOW", "60"))


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
s.settimeout(WINDOW + 2)

s.sendall(connect_packet("bambu-rs-delta", "bblp", CODE))
hdr = s.recv(4)
if not (len(hdr) >= 4 and hdr[0] == 0x20 and hdr[3] == 0):
    print("connect/auth failed", file=sys.stderr)
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


start = time.time()
deadline = start + WINDOW
count = 0
print(f"capturing report messages for {WINDOW:.0f}s — trigger the change now …", file=sys.stderr)
with open(OUT, "w") as f:
    while time.time() < deadline:
        try:
            h = read_byte()
            rl = read_remlen()
            body = read_n(rl)
        except (socket.timeout, EOFError):
            break
        if (h & 0xF0) != 0x30:  # only PUBLISH QoS0
            continue
        tlen = int.from_bytes(body[0:2], "big")
        payload = body[2 + tlen:]
        try:
            obj = json.loads(payload)
        except Exception:
            continue
        # Whether this looks like the full snapshot or a delta.
        is_full = obj.get("print", {}).get("command") == "push_status"
        rec = {"t_ms": round((time.time() - start) * 1000), "full": is_full, "msg": obj}
        f.write(json.dumps(rec, sort_keys=True) + "\n")
        f.flush()
        count += 1

print(f"OK wrote {count} message(s) to {OUT}", file=sys.stderr)
sys.exit(0 if count else 3)
