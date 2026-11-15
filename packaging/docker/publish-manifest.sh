#!/usr/bin/env bash
# Tags one variant as a multi-arch image from images already pushed by digest, adds OCI
# annotations to the index, and signs the result with cosign (keyless). Used by
# .github/workflows/ci.yml and images.yml.
#
#   IMAGE=ghcr.io/slukedev/trackside VERSION=1.2.3 REVISION=<commit> \
#     packaging/docker/publish-manifest.sh <variant> <tags-file> <digest>...
#
# <tags-file> holds one tag per line, as packaging/docker/image-tags.sh prints them. The first
# tag must be one that never moves, since the signed digest is read back through it.

set -euo pipefail

variant=${1:?usage: publish-manifest.sh <variant> <tags-file> <digest>...}
tags_file=${2:?usage: publish-manifest.sh <variant> <tags-file> <digest>...}
shift 2
[ "$#" -ge 1 ] || { echo "no digests given" >&2; exit 2; }
: "${IMAGE:?}" "${VERSION:?}" "${REVISION:?}"

case "$variant" in
  slim) description="Self-hosted music server for the Tracks app (slim: no ffmpeg)" ;;
  standard) description="Self-hosted music server for the Tracks app" ;;
  acquire) description="Self-hosted music server for the Tracks app (with ffmpeg, yt-dlp and Deno)" ;;
  *) echo "unknown variant: $variant" >&2; exit 2 ;;
esac

source_url="https://github.com/slukedev/tracks-companion"
args=(
  --annotation "index:org.opencontainers.image.title=Trackside"
  --annotation "index:org.opencontainers.image.description=$description"
  --annotation "index:org.opencontainers.image.source=$source_url"
  --annotation "index:org.opencontainers.image.url=$source_url"
  --annotation "index:org.opencontainers.image.licenses=GPL-3.0-only"
  --annotation "index:org.opencontainers.image.version=$VERSION"
  --annotation "index:org.opencontainers.image.revision=$REVISION"
  --annotation "index:org.opencontainers.image.created=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
)
first_tag=""
while IFS= read -r tag; do
  [ -n "$tag" ] || continue
  args+=(--tag "$IMAGE:$tag")
  first_tag=${first_tag:-$tag}
done < "$tags_file"
[ -n "$first_tag" ] || { echo "no tags in $tags_file" >&2; exit 2; }

sources=()
for digest in "$@"; do
  sources+=("$IMAGE@$digest")
done

docker buildx imagetools create "${args[@]}" "${sources[@]}"
digest=$(docker buildx imagetools inspect "$IMAGE:$first_tag" --format '{{json .Manifest}}' | jq -r .digest)
cosign sign --yes "$IMAGE@$digest"
echo "$variant: $IMAGE@$digest"
sed "s|^|  $IMAGE:|" "$tags_file"
