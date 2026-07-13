# Test certificates

A throwaway CA (`ca.pem` / `ca_key.pem`) and a `localhost` leaf certificate
(`cert.pem` / `key.pem`) signed by it, used only by the TLS integration tests
(`tests/h2_tls.rs`).  The server presents `cert.pem` + `key.pem`; the client
trusts `ca.pem` via `client_config_with_ca`.

Regenerate with:

```sh
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 \
  -keyout ca_key.pem -out ca.pem -days 3650 -nodes -subj "/CN=go-http test CA"
openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 \
  -keyout key.pem -nodes -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" -out leaf.csr
openssl x509 -req -in leaf.csr -CA ca.pem -CAkey ca_key.pem -CAcreateserial \
  -days 3650 -copy_extensions copyall -out cert.pem
rm leaf.csr ca.srl
```
