#!/usr/bin/env bash
#
# Teach Bambu Studio to trust a `bambu serve --emulate` relay.
#
# Studio will not talk to the relay over MQTT, however well the relay behaves:
# it verifies the printer's certificate against the CAs it ships and drops the
# connection with a TLS `UnknownCA` alert. Nothing we can generate chains to
# those CAs — that would need Bambu's private key — so the only way through is
# for the relay's own certificate to be a trust anchor on this machine.
#
# What actually does the verifying is `libbambu_networking.so`, a closed-source
# plugin Studio downloads into ~/.config/BambuStudio/plugins. It carries no CA
# of its own; it exports `bambu_network_set_cert_file` and is handed one. The
# only printer CA bundle on a normal install is the one below, which is why
# this script edits that file.
#
# Appending a self-signed certificate to the bundle makes it a trust anchor in
# its own right — OpenSSL accepts an exactly-matching self-signed certificate
# found in the store. No separate CA is needed.
#
# Nothing is taken away: the bundled BBL CAs stay exactly as they were, and the
# original is kept beside the bundle. Undo with:
#
#     sudo cp <bundle>.bambu-rs.orig <bundle>
#
# A Studio update replaces the bundle, so re-run this afterwards.
#
# Usage:
#     sudo tools/trust_relay_in_studio.sh                 # defaults
#     sudo CERT=/path/to/<serial>.cert.pem tools/trust_relay_in_studio.sh
#     sudo BUNDLE=/path/to/printer.cer     tools/trust_relay_in_studio.sh
#     RELAY=127.0.0.1:8883                                # what to verify against
#     DRY_RUN=1                                           # check, change nothing
set -euo pipefail

BUNDLE=${BUNDLE:-/opt/bambustudio-bin/resources/cert/printer.cer}
RELAY=${RELAY:-127.0.0.1:8883}
DRY_RUN=${DRY_RUN:-}

die() { echo "error: $*" >&2; exit 1; }
note() { echo "$*"; }

# Writing the bundle needs root, but the certificate belongs to whoever runs the
# relay — and under `sudo` both $HOME and $XDG_CONFIG_HOME are root's. Looking
# in /root/.config would be wrong every single time.
if [ -z "${CERT_DIR:-}" ]; then
  if [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != root ]; then
    sudo_home=$(getent passwd "$SUDO_USER" | cut -d: -f6)
    [ -n "$sudo_home" ] || die "cannot work out $SUDO_USER's home directory; set CERT= or CERT_DIR="
    CERT_DIR=$sudo_home/.config/bambu-rs/emulate
  else
    CERT_DIR=${XDG_CONFIG_HOME:-$HOME/.config}/bambu-rs/emulate
  fi
fi

command -v openssl >/dev/null || die "openssl is needed to read and check certificates"

# --- the relay's certificate -------------------------------------------------
# `bambu serve --emulate` writes one per serial and reuses it across restarts,
# which is what makes pinning it worthwhile in the first place.
if [ -z "${CERT:-}" ]; then
  shopt -s nullglob
  found=("$CERT_DIR"/*.cert.pem)
  shopt -u nullglob
  case ${#found[@]} in
    0)
      hint="run 'bambu serve --emulate' once, or set CERT="
      # `sudo -i` discards SUDO_USER, so there is no way to tell whose relay
      # this is; say that rather than let /root look like the user's mistake.
      [ "$CERT_DIR" = "/root/.config/bambu-rs/emulate" ] \
        && hint="that is root's home — with 'sudo -i' pass CERT=/home/<you>/.config/bambu-rs/emulate/<serial>.cert.pem, or use plain 'sudo'"
      die "no relay certificate under $CERT_DIR — $hint"
      ;;
    1) CERT=${found[0]} ;;
    *) die "several certificates under $CERT_DIR; pick one with CERT=: ${found[*]}" ;;
  esac
fi
[ -r "$CERT" ] || die "cannot read $CERT"
openssl x509 -noout -in "$CERT" 2>/dev/null || die "$CERT is not a PEM certificate"

subject=$(openssl x509 -noout -subject -in "$CERT")
note "relay certificate: $CERT"
note "  $subject"
note "  $(openssl x509 -noout -dates -in "$CERT" | tr '\n' ' ')"

# --- Studio's bundle ---------------------------------------------------------
[ -r "$BUNDLE" ] || die "no CA bundle at $BUNDLE (set BUNDLE= if Studio is installed elsewhere)"

# Compare by fingerprint rather than by text: the same certificate can be
# re-encoded, and a substring match would also fire on a partial write.
#
# Split first, one PEM block per file. `openssl x509` reads only the *first*
# certificate on a stream, so piping a whole bundle through it silently reports
# one fingerprint however many are in there — which would make the count check
# meaningless and the already-installed check miss unless ours happened to be
# first.
fingerprints() {
  local file=$1 dir f
  dir=$(mktemp -d) || return 1
  awk -v d="$dir" '/-----BEGIN CERTIFICATE-----/{n++} n{print >> (d "/c" n ".pem")}' "$file"
  for f in "$dir"/c*.pem; do
    [ -e "$f" ] || continue
    openssl x509 -noout -fingerprint -sha256 -in "$f" 2>/dev/null || true
  done
  rm -rf "$dir"
}
want=$(openssl x509 -noout -fingerprint -sha256 -in "$CERT")

if fingerprints "$BUNDLE" | grep -qxF "$want"; then
  note "already trusted in $BUNDLE — nothing to do"
  exit 0
fi

before=$(fingerprints "$BUNDLE" | wc -l)
note "bundle: $BUNDLE ($before certificate(s), none of them ours)"

if [ -n "$DRY_RUN" ]; then
  note "DRY_RUN set — would append the relay certificate and leave a backup at $BUNDLE.bambu-rs.orig"
  exit 0
fi

[ -w "$(dirname "$BUNDLE")" ] || die "need root to write $BUNDLE — re-run with sudo"

# --- build the new bundle, then check it before installing -------------------
tmp=$(mktemp) || die "mktemp failed"
trap 'rm -f "$tmp"' EXIT

cat "$BUNDLE" > "$tmp"
# Studio's printer.cer has no trailing newline. A plain append welds its last
# END line onto our BEGIN line, and OpenSSL then rejects the whole file — which
# would leave Studio unable to verify *any* printer, not just the relay.
[ -z "$(tail -c1 "$tmp")" ] || printf '\n' >> "$tmp"
cat "$CERT" >> "$tmp"

after=$(fingerprints "$tmp" | wc -l)
[ "$after" -eq "$((before + 1))" ] \
  || die "the new bundle parses as $after certificates, expected $((before + 1)) — refusing to install it"
fingerprints "$tmp" | grep -qxF "$want" || die "our certificate is not in the new bundle — refusing to install it"

cp -p "$BUNDLE" "$BUNDLE.bambu-rs.orig.$$"
if [ ! -e "$BUNDLE.bambu-rs.orig" ]; then
  mv "$BUNDLE.bambu-rs.orig.$$" "$BUNDLE.bambu-rs.orig"
  note "kept the original at $BUNDLE.bambu-rs.orig"
else
  rm -f "$BUNDLE.bambu-rs.orig.$$"
  note "original already saved at $BUNDLE.bambu-rs.orig (not overwritten)"
fi

cat "$tmp" > "$BUNDLE"   # write through, so the file keeps its owner and mode
note "added the relay certificate: $BUNDLE now holds $after"

# --- prove it, against the relay itself if it is up --------------------------
host=${RELAY%:*}; port=${RELAY##*:}
if timeout 3 bash -c "exec 3<>/dev/tcp/$host/$port" 2>/dev/null; then
  if timeout 10 openssl s_client -connect "$RELAY" -CAfile "$BUNDLE" </dev/null 2>&1 \
     | grep -q "Verify return code: 0 (ok)"; then
    note "verified: $RELAY now validates against this bundle"
  else
    note "WARNING: $RELAY still does not validate. Is the relay presenting $subject?"
    note "         Compare: openssl s_client -connect $RELAY | openssl x509 -noout -subject"
    exit 1
  fi
else
  note "note: nothing listening on $RELAY, so this was not verified end to end"
fi

note
note "Restart Bambu Studio and add the printer by IP."
note "A Studio update will replace the bundle; re-run this script if that happens."
