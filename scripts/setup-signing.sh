#!/usr/bin/env bash
# Create (or refresh) the self-signed code-signing identity used for local builds.
#
# Why: an ad-hoc signature changes its cdhash on every build, so macOS treats each build as a
# new app and forgets Screen Recording / Accessibility grants. A fixed certificate gives a
# stable designated requirement ("identifier + certificate hash"), so TCC permissions survive
# rebuilds. The certificate is local-only and not trusted by other machines (Gatekeeper will
# still warn there, as with ad-hoc).
#
# Layout (all user-scoped, no sudo):
#   ~/.macrdp/signing/macrdp-dev.{key,crt,p12}   key material
#   ~/.macrdp/signing/keychain.pass              password of the dedicated keychain
#   ~/Library/Keychains/macrdp-signing.keychain-db
#
# Safe to re-run: existing files and an existing identity are kept.
set -euo pipefail

IDENTITY="${MACRDP_SIGN_IDENTITY:-macrdp-dev}"
SIGN_DIR="$HOME/.macrdp/signing"
KEYCHAIN="$HOME/Library/Keychains/macrdp-signing.keychain-db"
LOGIN_KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"

mkdir -p "$SIGN_DIR"
chmod 700 "$SIGN_DIR"
umask 077

if [ ! -f "$SIGN_DIR/$IDENTITY.crt" ] || [ ! -f "$SIGN_DIR/$IDENTITY.key" ]; then
    echo "Generating self-signed code-signing certificate ($IDENTITY, 10 years)..."
    openssl req -x509 -newkey rsa:2048 \
        -keyout "$SIGN_DIR/$IDENTITY.key" \
        -out "$SIGN_DIR/$IDENTITY.crt" \
        -days 3650 -nodes \
        -subj "/CN=$IDENTITY/O=macrdp" \
        -addext "keyUsage=critical,digitalSignature" \
        -addext "extendedKeyUsage=critical,codeSigning" \
        -addext "basicConstraints=critical,CA:false" >/dev/null 2>&1
fi

# LibreSSL (macOS) produces the PBE-SHA1-3DES PKCS#12 that Security.framework accepts.
openssl pkcs12 -export -inkey "$SIGN_DIR/$IDENTITY.key" -in "$SIGN_DIR/$IDENTITY.crt" \
    -out "$SIGN_DIR/$IDENTITY.p12" -passout pass:"$IDENTITY" >/dev/null 2>&1

if [ ! -f "$SIGN_DIR/keychain.pass" ]; then
    openssl rand -hex 24 > "$SIGN_DIR/keychain.pass"
    chmod 600 "$SIGN_DIR/keychain.pass"
fi
PASS=$(cat "$SIGN_DIR/keychain.pass")

if ! security list-keychains -d user | grep -q "macrdp-signing.keychain"; then
    if [ ! -f "$KEYCHAIN" ]; then
        echo "Creating dedicated signing keychain..."
        security create-keychain -p "$PASS" "$KEYCHAIN"
        security set-keychain-settings "$KEYCHAIN"
    fi
    security list-keychains -d user -s "$LOGIN_KEYCHAIN" "$KEYCHAIN"
fi

security unlock-keychain -p "$PASS" "$KEYCHAIN"

if ! security find-identity -p codesigning "$KEYCHAIN" | grep -q "\"$IDENTITY\""; then
    security import "$SIGN_DIR/$IDENTITY.p12" -k "$KEYCHAIN" -P "$IDENTITY" \
        -T /usr/bin/codesign -T /usr/bin/security -A >/dev/null
    # Let codesign use the private key without a GUI prompt.
    security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$PASS" "$KEYCHAIN" >/dev/null
fi

security find-identity -p codesigning "$KEYCHAIN" | sed -n '2,4p'
echo "Signing identity '$IDENTITY' is ready."
echo "Unlock before building with: security unlock-keychain -p \"\$(cat $SIGN_DIR/keychain.pass)\" $KEYCHAIN"
