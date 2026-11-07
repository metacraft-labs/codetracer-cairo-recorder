#!/usr/bin/env bash
# Fetch a Starknet class's verified source from Voyager, under Source-Resolution.md §4a.
#
#   VOYAGER_API_KEY=… voyager-class-source.sh OUTDIR CLASS_HASH…
#   (normally through `just onchain-voyager-class-source`, which decrypts the key into
#   this one process and nowhere else)
#
# Each answer is stored as OUTDIR/<class_hash>.voyager.json, with its sha256 beside it,
# BEFORE anything parses it. What it holds is untrusted input: only `bind-class`'s
# class-hash comparison decides whether a build of it is shown.
#
# The §4a controls, applied to the one endpoint this fetch is allowed to call:
#   - the provider is FIXED (public-api.voyager.online over https); no part of the URL
#     comes from the chain except the class hash, which must be a felt;
#   - the host is resolved once, every address is checked against the private, loopback,
#     link-local, multicast, reserved and metadata ranges, and the connection is pinned
#     to the checked address (no rebinding between check and connect);
#   - redirects are not followed: any status but 200 or 404 is a failure;
#   - bytes and time are capped on what is actually read from the socket, not on a
#     declared length;
#   - the key travels only to that host, from stdin to curl, never on a command line.
set -euo pipefail

host="public-api.voyager.online"
max_bytes="${VOYAGER_MAX_BYTES:-$((32 * 1024 * 1024))}"
max_seconds=120

[ $# -ge 2 ] || { echo "usage: $0 OUTDIR CLASS_HASH…" >&2; exit 2; }
[ -n "${VOYAGER_API_KEY:-}" ] || { echo "VOYAGER_API_KEY is not set; run through 'just onchain-voyager-class-source'." >&2; exit 2; }
out="$1"; shift
mkdir -p "$out"

addr="$(python3 -I - "$host" <<'EOF'
import ipaddress, socket, sys
host = sys.argv[1]
addrs = sorted({ai[4][0] for ai in socket.getaddrinfo(host, 443, proto=socket.IPPROTO_TCP)})
if not addrs:
    sys.exit(f"{host}: no address")
for a in addrs:
    ip = ipaddress.ip_address(a)
    if not ip.is_global or ip.is_multicast or ip.is_reserved or a in ("169.254.169.254", "fd00:ec2::254"):
        sys.exit(f"{host} resolves to {a}, which is not a public address; refusing to connect")
print(next((a for a in addrs if ":" not in a), addrs[0]))
EOF
)"
case "$addr" in *:*) pin="[$addr]" ;; *) pin="$addr" ;; esac

status=0
for h in "$@"; do
  [[ "$h" =~ ^0x[0-9a-fA-F]{1,64}$ ]] || { echo "$h: not a class hash" >&2; status=1; continue; }
  dest="$out/$h.voyager.json"
  tmp="$(mktemp "$dest.tmp.XXXXXX")"; hdr="$tmp.headers"
  # head stops reading one byte past the cap, which makes curl stop too (exit 23).
  printf 'header = "x-api-key: %s"\n' "$VOYAGER_API_KEY" |
    curl -sS -K - --proto =https --proto-redir =https --max-redirs 0 --max-time "$max_seconds" \
      --resolve "$host:443:$pin" -D "$hdr" -o - "https://$host/beta/classes/$h/source" |
    head -c $((max_bytes + 1)) > "$tmp" && rc=0 || rc=$?
  size="$(stat -c %s "$tmp")"
  code="$(awk 'NR == 1 { print $2 }' "$hdr" 2>/dev/null || true)"
  rm -f "$hdr"
  if [ "$size" -gt "$max_bytes" ]; then
    echo "$h: answer exceeds $max_bytes bytes; discarded" >&2; rm -f "$tmp"; status=1; continue
  fi
  if [ "$rc" -ne 0 ]; then
    echo "$h: fetch failed" >&2; rm -f "$tmp"; status=1; continue
  fi
  case "$code" in
    200) mv "$tmp" "$dest"
         sha256sum "$dest" | sed "s#  .*#  $(basename "$dest")#" > "$dest.sha256"
         echo "$h: $size bytes -> $dest" ;;
    404) rm -f "$tmp"; echo "$h: not verified on Voyager (404)" ;;
    *)   rm -f "$tmp"; echo "$h: HTTP $code; nothing stored" >&2; status=1 ;;
  esac
done
exit "$status"
