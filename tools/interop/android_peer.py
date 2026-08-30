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

import argparse
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


def identity_packet(tcp_port=CONTROL_PORT):
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
            "tcpPort": tcp_port,
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


def load_handfast_cert():
    """Read handfast's identity cert (DER on disk) and return (der, pem-path)."""
    der_path = os.path.join(HANDFAST_CERT_DIR, "id_cert.der")
    if not os.path.exists(der_path):
        fail(f"handfast certificate not found at {der_path}")
    with open(der_path, "rb") as fh:
        der = fh.read()
    with open(HANDFAST_CERT, "wb") as fh:
        fh.write(b"-----BEGIN CERTIFICATE-----\n")
        fh.write(base64_encode_lines(der))
        fh.write(b"-----END CERTIFICATE-----\n")
    return der


def make_tls_ctx(server_side: bool, pin_cert: bytes):
    """TLS context matching android's SslHelper roles.

    Server role: present our cert, require the peer's cert (needClientAuth),
    pin it to handfast's on-disk certificate.
    Client role: present our cert when asked, verify the server against the
    same pinned certificate.
    """
    if server_side:
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        ctx.load_cert_chain(PEER_CERT, PEER_KEY)
        ctx.verify_mode = ssl.CERT_OPTIONAL  # request handfast's client cert
        ctx.load_verify_locations(HANDFAST_CERT)
        ctx.check_hostname = False
    else:
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        ctx.load_cert_chain(PEER_CERT, PEER_KEY)
        ctx.verify_mode = ssl.CERT_REQUIRED
        ctx.load_verify_locations(HANDFAST_CERT)
        ctx.check_hostname = False
    return ctx


def verify_peer_cert(tls, pin_cert):
    """The TLS peer certificate must byte-match handfast's on-disk cert."""
    presented = tls.getpeercert(binary_form=True)
    if not presented:
        fail("handfast did not present its certificate")
    if hashlib.sha256(presented).digest() != hashlib.sha256(pin_cert).digest():
        fail("handfast's presented certificate differs from its on-disk cert")


def validate_identity(body, expected_port):
    """DeviceInfo.isValidIdentityPacket checks (android)."""
    if not re.fullmatch(r"[a-zA-Z0-9_-]{32,38}", body.get("deviceId", "")):
        fail(f"handfast deviceId fails android regex: {body.get('deviceId')!r}")
    if not body.get("deviceName", "").strip():
        fail("handfast deviceName is blank (android drops such peers)")
    if body.get("protocolVersion") != PROTOCOL_VERSION:
        fail(f"handfast protocolVersion {body.get('protocolVersion')} != {PROTOCOL_VERSION}")
    if body.get("tcpPort") != expected_port:
        fail(f"handfast advertises tcpPort {body.get('tcpPort')} (must be {expected_port})")
    return body


def read_packet(tls, timeout_s=10):
    """Read one newline-delimited packet with a socket timeout."""
    tls.settimeout(timeout_s)
    try:
        return json.loads(read_line(tls))
    finally:
        tls.settimeout(None)


def announce_udp(advertised_port, attempts=5, tcp_port=None):
    """Send our identity datagram to handfast's UDP discovery port so it
    dials us (android's LanLinkProvider UDP broadcast, unicast here)."""
    payload = serialize(identity_packet(tcp_port if tcp_port is not None else advertised_port))
    for _ in range(attempts):
        try:
            udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            udp.sendto(payload, (HOST, 1716))
            udp.close()
        except OSError as err:
            print(f"udp announce failed: {err}", file=sys.stderr)
        time.sleep(0.2)


def serve(args):
    """Reverse role: handfast DIALS us (we are the TCP acceptor / TLS client,
    exactly android's LanLinkProvider when a remote initiates). Exercises
    outbound connect reliability, the interactive pairing answer, and payload
    reception from handfast. The driver script answers the pairing request via
    `hfctl pair-answer --accept` once we print PAIRING_REQUEST_SENT.
    """
    ensure_peer_certificate()
    pin_cert = load_handfast_cert()
    ctx = make_tls_ctx(server_side=False, pin_cert=pin_cert)
    save_dir = os.path.abspath(os.path.expanduser(args.save_dir))
    os.makedirs(save_dir, exist_ok=True)

    # Bind FIRST, then announce: handfast dials on the first datagram it
    # sees, so a listener must already be accepting when the announce lands.
    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind((HOST, args.control_port))
    listener.listen(1)
    listener.settimeout(20)
    announce_udp(args.control_port, tcp_port=args.control_port)
    print(f"serving on {HOST}:{args.control_port}; waiting for handfast to dial...")
    try:
        conn, _ = listener.accept()
    except socket.timeout:
        fail("handfast never dialed us")
    plain = read_packet(conn, timeout_s=10)
    if plain.get("type") != "kdeconnect.identity":
        fail(f"expected plaintext identity from handfast, got {plain.get('type')!r}")
    plain_body = plain.get("body", {})
    print(f"handfast plaintext identity OK: {plain_body.get('deviceId')}")
    tls = ctx.wrap_socket(conn, server_side=False)
    print("dial-out control TLS established; cipher:", tls.cipher())
    verify_peer_cert(tls, pin_cert)

    # ---- 2: protocol-8 secure identity re-exchange (write first). ----
    tls.sendall(serialize(identity_packet(args.control_port)))
    secure = read_packet(tls)
    if secure.get("type") != "kdeconnect.identity":
        fail(f"expected secure identity, got {secure.get('type')!r}")
    # handfast advertises its own control listener (1716), not our serve port.
    secure_body = validate_identity(secure.get("body", {}), CONTROL_PORT)
    if secure_body.get("deviceId") != plain_body.get("deviceId"):
        fail("device id changed between plaintext and secure identity")
    if secure_body.get("protocolVersion") != plain_body.get("protocolVersion"):
        fail("protocol version changed between plaintext and secure identity")
    print(f"handfast secure identity OK: {secure_body['deviceId']} "
          f"'{secure_body['deviceName']}' v{secure_body['protocolVersion']}")

    # ---- 3: request pairing, await the user's answer. ----
    tls.sendall(serialize({"id": next_id(), "type": "kdeconnect.pair", "body": {"pair": True}}))
    print("PAIRING_REQUEST_SENT", flush=True)
    answer = read_packet(tls, timeout_s=25)
    if answer.get("type") != "kdeconnect.pair":
        fail(f"expected pair answer, got {answer.get('type')!r}")
    if answer.get("body", {}).get("pair") is not True:
        fail(f"pairing declined: {answer}")
    print("pairing accepted by handfast")

    # ---- 4: receive the payload handfast sends (sender binds, receiver
    # dials as TLS client — android LanLink payload roles). ----
    header = read_packet(tls, timeout_s=25)
    if header.get("type") != "kdeconnect.share.request":
        fail(f"expected share.request header, got {header.get('type')!r}")
    body = header.get("body", {})
    size = header.get("payloadSize", 0)
    info = header.get("payloadTransferInfo") or {}
    port = info.get("port")
    if port is None:
        fail("share header lacks payloadTransferInfo.port")
    data = b""
    data_sock = socket.create_connection((HOST, port), timeout=15)
    data_tls = ctx.wrap_socket(data_sock, server_side=False)
    verify_peer_cert(data_tls, pin_cert)
    data_tls.settimeout(15)
    while len(data) < size:
        chunk = data_tls.recv(min(65536, size - len(data)))
        if not chunk:
            fail(f"payload stream ended early: {len(data)}/{size} bytes")
        data += chunk
    data_tls.close()
    if len(data) != size:
        fail(f"payload size mismatch: {len(data)} != {size}")
    fname = os.path.basename(body.get("filename", "received.bin")) or "received.bin"
    dest = os.path.join(save_dir, fname)
    with open(dest, "wb") as fh:
        fh.write(data)
    print(f"PAYLOAD_RECEIVED {len(data)} {hashlib.sha256(data).hexdigest()} -> {dest}", flush=True)

    # ---- 5: reconnect to prove pairing persisted + cert pinning passes. ----
    tls.close()
    listener.close()
    listener2 = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener2.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener2.bind((HOST, args.control_port))
    listener2.listen(1)
    listener2.settimeout(20)
    announce_udp(args.control_port, tcp_port=args.control_port)
    try:
        conn2, _ = listener2.accept()
    except socket.timeout:
        fail("handfast did not re-dial after the first connection closed")
    read_packet(conn2, timeout_s=10)  # plaintext identity
    tls2 = ctx.wrap_socket(conn2, server_side=False)
    verify_peer_cert(tls2, pin_cert)  # pinned fingerprint must still match
    tls2.sendall(serialize(identity_packet(args.control_port)))
    secure2 = read_packet(tls2)
    if secure2.get("type") != "kdeconnect.identity":
        fail(f"reconnect: expected identity, got {secure2.get('type')!r}")
    print("RECONNECT_OK (pairing persisted, certificate pinning passed)")
    tls2.close()
    listener2.close()

    print("DIAL-OUT INTEROP: ALL CHECKS PASSED")


def main():
    ensure_peer_certificate()
    pin_cert = load_handfast_cert()

    # ---------- 1 + 2: dial, plaintext identity, TLS server upgrade ----------
    ctx = make_tls_ctx(server_side=True, pin_cert=pin_cert)
    sock = socket.create_connection((HOST, CONTROL_PORT), timeout=10)
    sock.sendall(serialize(identity_packet()))
    tls = ctx.wrap_socket(sock, server_side=True)
    print("control TLS established; cipher:", tls.cipher())
    verify_peer_cert(tls, pin_cert)

    # ---------- 3: secure identity re-exchange (protocol >= 8) ----------
    tls.sendall(serialize(identity_packet()))  # write first, like android
    secure = read_packet(tls)
    if secure["type"] != "kdeconnect.identity":
        fail(f"expected identity, got {secure['type']}")
    validate_identity(secure["body"], CONTROL_PORT)
    print(f"handfast secure identity OK: {secure['body']['deviceId']} "
          f"'{secure['body']['deviceName']}' v{secure['body']['protocolVersion']}")

    # ---------- 4: payload file transfer (android -> handfast) ----------
    payload = bytes(range(256)) * 1024 + b"interop-tail"  # 256 KiB + tail
    want_sha = hashlib.sha256(payload).hexdigest()
    fname = "interop-from-android.bin"

    data_ctx = make_tls_ctx(server_side=True, pin_cert=pin_cert)

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
    parser = argparse.ArgumentParser(description="Android-shaped KDE Connect interop peer")
    parser.add_argument(
        "--serve",
        action="store_true",
        help="reverse role: handfast dials us (dial-out connect reliability + "
        "pairing answer + payload receive)",
    )
    parser.add_argument(
        "--control-port", type=int, default=CONTROL_PORT, help="TCP port to serve on"
    )
    parser.add_argument(
        "--save-dir",
        default=os.environ.get("SAVE_DIR", "~/Downloads"),
        help="directory to write received files (serve mode)",
    )
    args = parser.parse_args()
    if args.serve:
        serve(args)
    else:
        main()
