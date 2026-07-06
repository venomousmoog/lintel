#!/usr/bin/env bash
# Create a STABLE self-signed code-signing identity ("Lintel Dev") in your login
# keychain. macOS ties the Accessibility (TCC) grant to an app's code identity;
# an ad-hoc build gets a new identity every rebuild, so the grant never persists
# (you get re-prompted each launch). Signing every build with this stable cert
# keeps the identity constant, so the grant sticks.
#
# Run ONCE:  make cert     (or: bash scripts/make-signing-cert.sh)
# It may pop a one-time login-keychain password prompt. Safe; only adds a cert.
set -euo pipefail

NAME="${1:-Lintel Dev}"

# A self-signed cert is untrusted, so it never appears under `find-identity -v`
# even though codesign can use it. Detect existence by cert common-name instead.
if security find-certificate -c "$NAME" >/dev/null 2>&1; then
  echo "Identity '$NAME' already exists — nothing to do."
  exit 0
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

cat > "$TMP/openssl.cnf" <<EOF
[req]
distinguished_name = dn
x509_extensions = v3
prompt = no
[dn]
CN = $NAME
[v3]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
EOF

echo "==> generating self-signed code-signing cert '$NAME'"
openssl req -x509 -newkey rsa:2048 -nodes \
  -keyout "$TMP/key.pem" -out "$TMP/cert.pem" \
  -days 3650 -config "$TMP/openssl.cnf" >/dev/null 2>&1

# `security import` fails the p12 MAC check with an EMPTY password, so use a
# throwaway one. OpenSSL 3.x needs `-legacy` for an Apple-readable MAC; LibreSSL
# (system openssl) has no `-legacy` but already writes the compatible format.
P12PASS="lintel"
if ! openssl pkcs12 -export -legacy -inkey "$TMP/key.pem" -in "$TMP/cert.pem" \
      -name "$NAME" -out "$TMP/id.p12" -passout "pass:$P12PASS" >/dev/null 2>&1; then
  openssl pkcs12 -export -inkey "$TMP/key.pem" -in "$TMP/cert.pem" \
    -name "$NAME" -out "$TMP/id.p12" -passout "pass:$P12PASS" >/dev/null 2>&1
fi

echo "==> importing into the login keychain (may prompt for your password)"
security import "$TMP/id.p12" \
  -k "$HOME/Library/Keychains/login.keychain-db" \
  -P "$P12PASS" -T /usr/bin/codesign -A

echo "==> verifying (probe-sign a throwaway file)"
PROBE="$TMP/probe"; printf 'x' > "$PROBE"
if codesign --force --sign "$NAME" "$PROBE" >/dev/null 2>&1; then
  echo
  echo "Done — codesign can sign with '$NAME'. Next:"
  echo "  1) make run      # now codesigns Lintel.app with '$NAME'"
  echo "  2) System Settings > Privacy & Security > Accessibility: remove any stale"
  echo "     'Lintel' entry (select, press -), then grant the freshly-signed Lintel once."
  echo "  The grant now persists across rebuilds."
else
  echo "WARNING: codesign could not use '$NAME'; open Keychain Access to check," >&2
  echo "or create it via Certificate Assistant instead." >&2
  exit 1
fi
