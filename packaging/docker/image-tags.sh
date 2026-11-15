#!/bin/sh
# Prints the image tags for one variant, one per line. Used by .github/workflows/ci.yml and
# images.yml.
#
#   packaging/docker/image-tags.sh <slim|standard|acquire> edge <commit>
#   packaging/docker/image-tags.sh <slim|standard|acquire> release <X.Y.Z[-pre]> <latest: true|false>
#
# standard is the unsuffixed default; slim and acquire add -slim and -acquire. A release that
# is GitHub's latest also gets latest, latest-<variant> and the older bare slim and acquire.
# The first line is the one tag that never moves.

set -eu

usage() {
  echo "usage: image-tags.sh <slim|standard|acquire> edge <commit>" >&2
  echo "       image-tags.sh <slim|standard|acquire> release <version> <true|false>" >&2
  exit 2
}

variant=${1:-}
kind=${2:-}
case "$variant" in
  standard) suffix="" ;;
  slim | acquire) suffix="-$variant" ;;
  *) usage ;;
esac

case "$kind" in
  edge)
    commit=${3:-}
    [ -n "$commit" ] || usage
    short=$(printf '%s' "$commit" | cut -c1-7)
    echo "edge-$short$suffix"
    echo "edge$suffix"
    ;;
  release)
    version=${3:-}
    latest=${4:-}
    echo "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$' || {
      echo "not a release version: $version" >&2
      exit 1
    }
    case "$latest" in true | false) ;; *) usage ;; esac
    echo "$version$suffix"
    case "$version" in *-*) exit 0 ;; esac
    major=${version%%.*}
    minor=${version%.*}
    echo "$minor$suffix"
    if [ "$major" -ge 1 ]; then echo "$major$suffix"; fi
    if [ "$latest" = true ]; then
      echo "latest$suffix"
      if [ -n "$suffix" ]; then echo "$variant"; fi
    fi
    ;;
  *) usage ;;
esac
