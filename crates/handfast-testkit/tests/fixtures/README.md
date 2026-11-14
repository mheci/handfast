# TLS test fixtures

This directory is test-only. It holds two self-signed RSA-2048 certificate and
key pairs.

- `rsa-cert.der` has CN `rsa0rsa0rsa0rsa0rsa0rsa0rsa0rsa0` and matches
  `RSA_DEVICE_ID` in `src/tls_peer.rs`.
- `rsa-wrong-cert.der` has CN `rsa1rsa1rsa1rsa1rsa1rsa1rsa1rsa1` and matches
  `RSA_WRONG_DEVICE_ID`. Tests use it as the wrong RSA certificate.

`rsa-key.der` and `rsa-wrong-key.der` are the unencrypted PKCS#8 private keys
for their certificates.

The first pair was generated on 2026-11-13 with OpenSSL 3.5.9:

```sh
openssl req -x509 -newkey rsa:2048 -sha256 -nodes -days 3650 \
  -subj "/CN=rsa0rsa0rsa0rsa0rsa0rsa0rsa0rsa0" \
  -keyout rsa-key.pem -out rsa-cert.pem
openssl pkcs8 -topk8 -nocrypt -in rsa-key.pem -outform DER -out rsa-key.der
openssl x509 -in rsa-cert.pem -outform DER -out rsa-cert.der
```

The second pair was generated on 2026-11-14 with OpenSSL 3.5.9:

```sh
openssl req -x509 -newkey rsa:2048 -sha256 -nodes -days 3650 \
  -subj "/CN=rsa1rsa1rsa1rsa1rsa1rsa1rsa1rsa1" \
  -keyout rsa-wrong-key.pem -out rsa-wrong-cert.pem
openssl pkcs8 -topk8 -nocrypt -in rsa-wrong-key.pem -outform DER -out rsa-wrong-key.der
openssl x509 -in rsa-wrong-cert.pem -outform DER -out rsa-wrong-cert.der
```

Regenerate each pair together. The certificate CN must stay equal to its device
ID constant in `src/tls_peer.rs`. Never use these keys outside tests.
