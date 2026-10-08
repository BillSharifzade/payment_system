#!/usr/bin/env bash
# Print the registry digest an image tag currently points to, for pinning
# `image: repo:tag@sha256:...` (compose files, Dockerfiles, CI).
#
#   scripts/image-digest.sh postgres:16.15 caddy:2.11.7 gcr.io/distroless/cc-debian12:nonroot
#
# Docker Hub (library/ implied for bare names), gcr.io and quay.io. Prints the
# multi-arch index digest when the tag has one. Needs curl and jq.
set -euo pipefail
accept='application/vnd.oci.image.index.v1+json,application/vnd.docker.distribution.manifest.list.v2+json,application/vnd.docker.distribution.manifest.v2+json,application/vnd.oci.image.manifest.v1+json'
[[ $# -gt 0 ]] || { sed -n '2,8p' "$0"; exit 2; }
digest_header() { tr -d '\r' | awk -F': ' 'tolower($1) == "docker-content-digest" { print $2 }'; }
for ref in "$@"; do
  name=${ref%:*}; tag=${ref##*:}
  [[ "$name" != "$ref" ]] || tag=latest
  case "$name" in
    gcr.io/*|quay.io/*)
      host=${name%%/*}; path=${name#*/}
      d=$(curl -fsSI -H "Accept: $accept" "https://$host/v2/$path/manifests/$tag" | digest_header) ;;
    *)
      [[ "$name" == */* ]] || name="library/$name"
      tok=$(curl -fsS "https://auth.docker.io/token?service=registry.docker.io&scope=repository:$name:pull" | jq -r .token)
      d=$(curl -fsSI -H "Authorization: Bearer $tok" -H "Accept: $accept" \
            "https://registry-1.docker.io/v2/$name/manifests/$tag" | digest_header) ;;
  esac
  echo "$ref@${d:?could not resolve $ref}"
done
