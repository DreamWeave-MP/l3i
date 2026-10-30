A P-256 key made for these tests and published with them, as SEC1 PEM (`EC PRIVATE KEY`, the form
OpenSSL and most token issuers hand out), SEC1 DER and PKCS#8 PEM, and a P-384 key as PKCS#8 PEM.
ECDSA signatures are randomized, so `tests/bytes.rs` verifies `bytes.ecdsaSign`'s output against
each key's public point (`openssl ec -pubout`) instead of comparing bytes. Never use these keys for
anything else.
