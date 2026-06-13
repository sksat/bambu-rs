#!/usr/bin/env python3
# /// script
# requires-python = ">=3.9"
# dependencies = []
# ///
"""Clean-room capture of `info.get_version` (stdlib only, read-only)."""
import socket, ssl, os, json, sys, time
IP=os.environ["BAMBU_IP"]; SERIAL=os.environ["BAMBU_SERIAL"]; CODE=os.environ["BAMBU_CODE"]
def elen(n):
    o=bytearray()
    while True:
        b=n%128; n//=128
        if n>0: b|=0x80
        o.append(b)
        if n==0: break
    return bytes(o)
def estr(s):
    b=s.encode() if isinstance(s,str) else s
    return len(b).to_bytes(2,"big")+b
def conn(cid,u,p):
    vh=estr("MQTT")+bytes([0x04,0xC2])+(60).to_bytes(2,"big"); pl=estr(cid)+estr(u)+estr(p)
    return bytes([0x10])+elen(len(vh)+len(pl))+vh+pl
def sub(pid,t): vh=pid.to_bytes(2,"big")+estr(t)+bytes([0]); return bytes([0x82])+elen(len(vh))+vh
def pub(t,p): b=estr(t)+p; return bytes([0x30])+elen(len(b))+b
ctx=ssl.create_default_context(); ctx.check_hostname=False; ctx.verify_mode=ssl.CERT_NONE
s=ctx.wrap_socket(socket.create_connection((IP,8883),timeout=10),server_hostname=IP); s.settimeout(12)
s.sendall(conn("bambu-rs-getver","bblp",CODE)); h=s.recv(4)
if not(len(h)>=4 and h[0]==0x20 and h[3]==0): print("connack fail",h.hex(),file=sys.stderr); sys.exit(2)
s.sendall(sub(1,f"device/{SERIAL}/report"))
s.sendall(pub(f"device/{SERIAL}/request", json.dumps({"info":{"sequence_id":"0","command":"get_version"}}).encode()))
def rb():
    b=s.recv(1)
    if not b: raise EOFError
    return b[0]
def rlen():
    m,v=1,0
    while True:
        b=rb(); v+=(b&0x7f)*m
        if not(b&0x80): break
        m*=128
    return v
def rn(n):
    buf=bytearray()
    while len(buf)<n:
        c=s.recv(n-len(buf))
        if not c: raise EOFError
        buf+=c
    return bytes(buf)
found=None; deadline=time.time()+12
while time.time()<deadline:
    try:
        hh=rb(); rl=rlen(); body=rn(rl)
    except (socket.timeout,EOFError): break
    if (hh&0xF0)==0x30:
        tl=int.from_bytes(body[0:2],"big"); pay=body[2+tl:]
        try: obj=json.loads(pay)
        except: continue
        if isinstance(obj,dict) and "info" in obj and obj["info"].get("command")=="get_version":
            found=obj["info"]; break
if not found: print("NO_GET_VERSION",file=sys.stderr); sys.exit(3)
json.dump(found, open("captures/get_version-raw.json","w"), indent=2)
print("modules (sn redacted):")
for m in found.get("module",[]):
    print(f"  {m.get('name'):<16} sw_ver={m.get('sw_ver','?'):<14} hw_ver={m.get('hw_ver','?')}")
