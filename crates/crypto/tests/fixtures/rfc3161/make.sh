#!/usr/bin/env bash
# Regenerates the OpenSSL-issued RFC 3161 fixtures: an RSA-4096 root, an
# RSA-2048 TSA certificate (critical EKU timeStamping) and two responses from
# `openssl ts -reply` (with and without the TSA certificate embedded), ESS
# SigningCertificate v1 (SHA-1 cert id) as OpenSSL emits by default. They are
# an independent implementation's tokens for crates/crypto/src/rfc3161.rs.
#
#   crates/crypto/tests/fixtures/rfc3161/make.sh      (needs openssl >= 3)
set -euo pipefail
cd "$(dirname "$0")"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

cat > "$work/openssl.cnf" <<EOF
[ req ]
distinguished_name = dn
[ dn ]
[ root_ext ]
basicConstraints = critical, CA:true
keyUsage = critical, keyCertSign, cRLSign
subjectKeyIdentifier = hash
[ tsa_ext ]
basicConstraints = critical, CA:false
keyUsage = critical, digitalSignature
extendedKeyUsage = critical, timeStamping
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid
[ tsa ]
default_tsa = tsa_config
[ tsa_config ]
serial = $work/serial
signer_cert = $work/tsa.pem
signer_key = $work/tsa.key
signer_digest = sha256
default_policy = 1.3.6.1.4.1.99999.1
digests = sha256
accuracy = secs:1
ordering = no
tsa_name = no
ess_cert_id_chain = no
EOF

openssl req -x509 -new -newkey rsa:4096 -nodes -sha256 -days 36500 \
  -keyout "$work/root.key" -out root.pem -subj "/O=Fixture/CN=Fixture TSA Root" \
  -config "$work/openssl.cnf" -extensions root_ext 2>/dev/null
openssl req -new -newkey rsa:2048 -nodes -keyout "$work/tsa.key" -out "$work/tsa.csr" \
  -subj "/O=Fixture/CN=Fixture TSA" -config "$work/openssl.cnf" 2>/dev/null
openssl x509 -req -in "$work/tsa.csr" -CA root.pem -CAkey "$work/root.key" -set_serial 2 \
  -days 36500 -sha256 -extfile "$work/openssl.cnf" -extensions tsa_ext -out tsa.pem 2>/dev/null
cp tsa.pem "$work/tsa.pem"
echo 01 > "$work/serial"

printf 'payment-system checkpoint fixture\n' > datum.bin
openssl ts -query -data datum.bin -sha256 -cert -out "$work/req.tsq"
openssl ts -query -in "$work/req.tsq" -text | sed -n 's/^Nonce: 0x//p' > nonce.txt
openssl ts -reply -config "$work/openssl.cnf" -queryfile "$work/req.tsq" -out response.tsr 2>/dev/null
openssl ts -query -data datum.bin -sha256 -out "$work/req-nocert.tsq"
openssl ts -reply -config "$work/openssl.cnf" -queryfile "$work/req-nocert.tsq" -out response-nocert.tsr 2>/dev/null

# OpenSSL must accept its own output, or the fixtures prove nothing.
openssl ts -verify -data datum.bin -in response.tsr -CAfile root.pem
echo "fixtures written to $(pwd)"
