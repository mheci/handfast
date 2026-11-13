# TLS test fixtures

This directory is test-only. `rsa-cert.der` is a self-signed RSA-2048
certificate with CN `rsa0rsa0rsa0rsa0rsa0rsa0rsa0rsa0`, and `rsa-key.der` is its
unencrypted PKCS#8 private key.

Generated on 2026-11-13 with OpenSSL 3.5.9:

```sh
openssl req -x509 -newkey rsa:2048 -sha256 -nodes -days 3650 \
  -subj "/CN=rsa0rsa0rsa0rsa0rsa0rsa0rsa0rsa0" \
  -keyout rsa-key.pem -out rsa-cert.pem
openssl pkcs8 -topk8 -nocrypt -in rsa-key.pem -outform DER -out rsa-key.der
openssl x509 -in rsa-cert.pem -outform DER -out rsa-cert.der
```

Regenerate both files together. The certificate CN must stay equal to
`RSA_DEVICE_ID` in `src/tls_peer.rs`. Never use this key outside tests.
