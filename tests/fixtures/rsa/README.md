A 2048-bit RSA key made for these tests and published with them, as PKCS#1 PEM, PKCS#8 PEM and
PKCS#8 DER, and a P-256 key that `bytes.rsaSign` must refuse. The expected signatures in
`tests/bytes.rs` are OpenSSL's: `openssl dgst -sha256 -sign pkcs8.pem` over `header.payload`.
Never use these keys for anything else.
