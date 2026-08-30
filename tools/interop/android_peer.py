#!/usr/bin/env python3
"""Independent Android-shaped KDE Connect peer — interop smoke against handfastd.

This is deliberately NOT handfast's own code: it re-implements the upstream
android LanLinkProvider / LanLink wire behavior from the public protocol
(KDE/kdeconnect-android master) and drives a real handfastd through it:

  1. TCP dial to handfastd:1716, plaintext `kdeconnect.identity` line first.
  2. Upgrade the SAME socket to TLS as the *server* (android's rule:
     "if I'm the TCP server I will be the SSL client and vice-versa"; the
     dialer plays TLS server), presenting an RSA-2048 cert whose CN is our
     device id and requesting/verifying handfast's client cert — exactly
     android's trusted-device needClientAuth mode.
  3. Protocol-8 secure identity re-exchange inside TLS; validate handfast's
     identity exactly like DeviceInfo.isValidIdentityPacket.
  4. Payload file transfer TO handfast: `kdeconnect.share.request` header
     with payloadSize + payloadTransferInfo{port}, TLS-server accept on the
     data socket, stream raw bytes (android sendPayload, 4096 chunks).
  5. Header-only empty-file share (android: no payload for size 0).

Exits 0 only if every step byte-verifies.

Usage:
    HANDFAST_CERT_DIR=~/.config/handfast SAVE_DIR=~/Downloads python3 android_peer.py

The peer certificate (RSA-2048, CN = device id, OU "KDE Connect") is
generated on first run with the `openssl` CLI when missing.
"""

import hashlib
import json
import os
import re
import socket
import ssl
import subprocess
import sys
import time

HOST = "127.0.0.1"
CONTROL_PORT = 1716
PAYLOAD_MIN_PORT = 1739
DEVICE_ID = "interoptest0000000000000000000000"  # 32 chars: ^[a-zA-Z0-9_-]{32,38}$
DEVICE_NAME = "Interop Tester"
PROTOCOL_VERSION = 8
MAX_IDENTITY_PACKET_SIZE = 512 * 1024

HERE = os.path.dirname(os.path.abspath(__file__))
HANDFAST_CERT_DIR = os.environ.get("HANDFAST_CERT_DIR", os.path.expanduser("~/.config/handfast"))
SAVE_DIR = os.path.expanduser(os.environ.get("SAVE_DIR", "~/Downloads"))
PEER_CERT = os.path.join(HERE, "peer-cert.pem")
PEER_KEY = os.path.join(HERE, "peer-key.pem")
HANDFAST_CERT = os.path.join(HERE, "handfast-cert.pem")

_pkt_id = 0


def next_id():
    global _pkt_id
    _pkt_id += 1
    return _pkt_id


def ensure_peer_certificate():
    """Generate the Android-style device certificate if missing."""
    if os.path.exists(PEER_CERT) and os.path.exists(PEER_KEY):
        return
    subprocess.run(
        [
            "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
            "-keyout", PEER_KEY, "-out", PEER_CERT, "-days", "3650",
            "-subj",
            f"/CN={DEVICE_ID}/OU=KDE Connect/O=KDE",
        ],
        check=True,
        capture_output=True,
    )


def identity_packet():
    return {
        "id": next_id(),
        "type": "kdeconnect.identity",
        "body": {
            "deviceId": DEVICE_ID,
            "deviceName": DEVICE_NAME,
            "protocolVersion": PROTOCOL_VERSION,
            "deviceType": "phone",
            "incomingCapabilities": ["kdeconnect.ping", "kdeconnect.share.request"],
            "outgoingCapabilities": ["kdeconnect.ping", "kdeconnect.share.request"],
            "tcpPort": CONTROL_PORT,
        },
    }


def serialize(packet):
    return json.dumps(packet, separators=(",", ":"), ensure_ascii=False).encode() + b"\n"


def read_line(sock, maxlen=MAX_IDENTITY_PACKET_SIZE):
    """Byte-by-byte bounded line read — android LanLink's handshake read."""
    buf = b""
    while True:
        byte = sock.recv(1)
        if not byte:
            raise EOFError("peer closed mid-line")
        if byte == b"\n":
            return buf
        buf += byte
        if len(buf) > maxlen:
            raise ValueError("line exceeds bound")


def fail(msg):
    print(f"FAIL: {msg}", file=sys.stderr)
    sys.exit(1)


def main():
    ensure_peer_certificate()

    handfast_der = os.path.join(HANDFAST_CERT_DIR, "id_cert.der")
    if not os.path.exists(handfast_der):
        fail(f"handfast certificate not found at {handfast_der}")
    with open(handfast_der, "rb") as fh:
        disk_der = fh.read()
    # PEM copy for the TLS trust store (self-signed cert used as its own root).
    with open(HANDFAST_CERT, "wb") as fh:
        fh.write(b"-----BEGIN CERTIFICATE-----\n")
        fh.write(base64_encode_lines(disk_der))
        fh.write(b"-----END CERTIFICATE-----\n")

    # ---------- 1 + 2: dial, plaintext identity, TLS server upgrade ----------
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(PEER_CERT, PEER_KEY)
    ctx.verify_mode = ssl.CERT_OPTIONAL  # request handfast's client cert (android needClientAuth)
    ctx.load_verify_locations(HANDFAST_CERT)
    ctx.check_hostname = False

    sock = socket.create_connection((HOST, CONTROL_PORT), timeout=10)
    sock.sendall(serialize(identity_packet()))
    tls = ctx.wrap_socket(sock, server_side=True)
    print("control TLS established; cipher:", tls.cipher())

    peer_cert = tls.getpeercert(binary_form=True)
    if not peer_cert:
        fail("handfast did not present its client certificate")
    if hashlib.sha256(peer_cert).digest() != hashlib.sha256(disk_der).digest():
        fail("handfast's presented client cert differs from its on-disk cert")

    # ---------- 3: secure identity re-exchange (protocol >= 8) ----------
    tls.sendall(serialize(identity_packet()))  # write first, like android
    secure = json.loads(read_line(tls))
    if secure["type"] != "kdeconnect.identity":
        fail(f"expected identity, got {secure['type']}")
    body = secure["body"]
    if not re.fullmatch(r"[a-zA-Z0-9_-]{32,38}", body.get("deviceId", "")):
        fail(f"handfast deviceId fails android regex: {body.get('deviceId')!r}")
    if not body.get("deviceName", "").strip():
        fail("handfast deviceName is blank (android drops such peers)")
    if body.get("protocolVersion") != PROTOCOL_VERSION:
        fail(f"handfast protocolVersion {body.get('protocolVersion')} != {PROTOCOL_VERSION}")
    if body.get("tcpPort") != CONTROL_PORT:
        fail(f"handfast advertises tcpPort {body.get('tcpPort')} (must be {CONTROL_PORT})")
    print(f"handfast secure identity OK: {body['deviceId']} '{body['deviceName']}' "
          f"v{body['protocolVersion']}")

    # ---------- 4: payload file transfer (android -> handfast) ----------
    payload = bytes(range(256)) * 1024 + b"interop-tail"  # 256 KiB + tail
    want_sha = hashlib.sha256(payload).hexdigest()
    fname = "interop-from-android.bin"

    data_ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    data_ctx.load_cert_chain(PEER_CERT, PEER_KEY)
    data_ctx.verify_mode = ssl.CERT_OPTIONAL
    data_ctx.load_verify_locations(HANDFAST_CERT)
    data_ctx.check_hostname = False

    listener = None
    for port in range(PAYLOAD_MIN_PORT, PAYLOAD_MIN_PORT + 26):  # android range 1739..1764
        try:
            listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            listener.bind((HOST, port))
            break
        except OSError:
            listener.close()
            listener = None
    if listener is None:
        fail("no free payload port in 1739..1764")
    listener.listen(1)
    pport = listener.getsockname()[1]
    print(f"payload listener on port {pport}")

    tls.sendall(
        serialize(
            {
                "id": next_id(),
                "type": "kdeconnect.share.request",
                "body": {"filename": fname},
                "payloadSize": len(payload),
                "payloadTransferInfo": {"port": pport},
            }
        )
    )

    data_sock, _ = listener.accept()
    data_tls = data_ctx.wrap_socket(data_sock, server_side=True)
    view = memoryview(payload)
    for off in range(0, len(payload), 4096):  # android sendPayload chunking
        data_tls.sendall(view[off : off + 4096])
    data_tls.close()
    listener.close()
    print(f"payload streamed: {len(payload)} bytes")

    got = None
    for _ in range(100):
        path = os.path.join(SAVE_DIR, fname)
        if os.path.exists(path):
            got = path
            break
        time.sleep(0.05)
    if got is None:
        fail(f"handfast never wrote {fname} into {SAVE_DIR}")
    with open(got, "rb") as fh:
        data = fh.read()
    if len(data) != len(payload):
        fail(f"payload size mismatch: {len(data)} != {len(payload)}")
    if hashlib.sha256(data).hexdigest() != want_sha:
        fail("payload bytes corrupted in transit")
    print(f"payload file OK: {got} ({len(data)} bytes, sha256 matches)")

    # ---------- 5: header-only empty-file share ----------
    empty_name = "interop-empty.txt"
    tls.sendall(
        serialize(
            {
                "id": next_id(),
                "type": "kdeconnect.share.request",
                "body": {"filename": empty_name, "fileSize": 0},
                "payloadSize": 0,
            }
        )
    )
    got_empty = None
    for _ in range(100):
        path = os.path.join(SAVE_DIR, empty_name)
        if os.path.exists(path):
            got_empty = path
            break
        time.sleep(0.05)
    if got_empty is None:
        fail(f"handfast never created {empty_name}")
    if os.path.getsize(got_empty) != 0:
        fail(f"empty share produced a {os.path.getsize(got_empty)}-byte file")
    print(f"empty-file share OK: {got_empty} (0 bytes)")

    tls.close()
    print("INTEROP SMOKE: ALL CHECKS PASSED")


def base64_encode_lines(der: bytes) -> bytes:
    import base64
    encoded = base64.b64encode(der)
    lines = [encoded[i : i + 64] for i in range(0, len(encoded), 64)]
    return b"\n".join(lines) + b"\n"


if __name__ == "__main__":
    main()
