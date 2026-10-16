# Test fixtures: throwaway mTLS PKI

These files are **test-only** material for `tests/outbound_mtls_test.rs` and the
unit tests in `src/outbound_tls.rs`. They were generated ad hoc with openssl:

```
openssl ecparam -name prime256v1 -genkey -noout -out ca.key
openssl req -new -x509 -key ca.key -out ca.crt -days 3650 -subj "/CN=Ulnclaw Test CA"
openssl ecparam -name prime256v1 -genkey -noout -out server.key
openssl req -new -key server.key -out server.csr -subj "/CN=127.0.0.1" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1"
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
  -out server.crt -days 3650 -copy_extensions copy
openssl ecparam -name prime256v1 -genkey -noout -out client.key
openssl req -new -key client.key -out client.csr -subj "/CN=ulnclaw-test-client"
openssl x509 -req -in client.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
  -out client.crt -days 3650
```

They are **not** credentials: the CA signs nothing but the local loopback test
server, and the key material exists only inside this repository and the test
process. Never reuse them outside tests.