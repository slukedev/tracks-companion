#!/bin/sh
# Starts an image with no configuration on a read-only root filesystem and checks what a new
# self-hoster would see: the setup token in the log, a passing healthcheck, and the tools
# that variant promises.
#
#   packaging/docker/smoke-test.sh <image> <slim|standard|acquire>

set -eu

image=${1:?usage: smoke-test.sh <image> <slim|standard|acquire>}
variant=${2:?usage: smoke-test.sh <image> <slim|standard|acquire>}
name="trackside-smoke-$$"
timeout_s=${SMOKE_TIMEOUT:-90}

cleanup() {
  docker rm -f "$name" >/dev/null 2>&1 || true
  docker volume rm "$name-data" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

fail() {
  echo "FAIL: $*" >&2
  docker logs "$name" >&2 2>&1 || true
  exit 1
}

user=$(docker image inspect --format '{{.Config.User}}' "$image")
[ "$user" = "65532:65532" ] || fail "image user is '$user', expected 65532:65532"

docker volume create "$name-data" >/dev/null
docker run -d --name "$name" \
  --read-only \
  --cap-drop ALL \
  --security-opt no-new-privileges \
  -v "$name-data:/data" \
  -p 127.0.0.1::4747 \
  "$image" >/dev/null

elapsed=0
status=starting
while [ "$elapsed" -lt "$timeout_s" ]; do
  status=$(docker inspect --format '{{.State.Health.Status}}' "$name" 2>/dev/null || echo gone)
  case "$status" in
    healthy) break ;;
    unhealthy | gone) fail "container is $status" ;;
  esac
  sleep 2
  elapsed=$((elapsed + 2))
done
[ "$status" = healthy ] || fail "not healthy after ${timeout_s}s (status: $status)"
echo "ok: healthcheck passes"

logs=$(docker logs "$name" 2>&1)
echo "$logs" | grep -q "setup token" || fail "no setup token in the log"
echo "ok: setup token printed"

docker exec "$name" /usr/local/bin/trackside healthcheck >/dev/null || fail "trackside healthcheck exited non-zero"

port=$(docker port "$name" 4747/tcp | head -n 1 | sed 's/.*://')
ready=$(curl -fsS "http://127.0.0.1:$port/readyz") || fail "/readyz did not answer"
echo "ok: /readyz $ready"

case "$variant" in
  slim)
    if echo "$ready" | grep -q '"ffmpeg": *"ok"'; then fail "slim image reports ffmpeg"; fi
    echo "ok: no ffmpeg, as expected"
    ;;
  standard)
    echo "$ready" | grep -q '"ffmpeg": *"ok"' || fail "standard image does not report ffmpeg"
    echo "ok: ffmpeg found"
    ;;
  acquire)
    echo "$ready" | grep -q '"ffmpeg": *"ok"' || fail "acquire image does not report ffmpeg"
    docker exec "$name" /opt/yt-dlp/yt-dlp --version >/dev/null || fail "yt-dlp does not run"
    docker exec "$name" /usr/local/bin/deno --version >/dev/null || fail "deno does not run"
    echo "ok: ffmpeg, yt-dlp and deno run"
    ;;
  *) fail "unknown variant $variant" ;;
esac

echo "PASS: $image ($variant)"
