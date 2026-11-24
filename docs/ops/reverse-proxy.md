# Reverse proxies

A proxy in front of Trackside has three jobs: terminate TLS, pass through long responses
(songs, HLS segments, the live event stream) without buffering them, and tell Trackside who
the client really is.

On the Trackside side, set the proxy's address as trusted, so `X-Forwarded-For`,
`X-Forwarded-Proto` and `X-Forwarded-Host` are believed. Headers from anyone else are ignored,
and a proxy that isn't trusted is judged by its own address alone: since it could be passing on
requests from the internet, passwords and keys that come through it over plain HTTP are refused
with `https_required`, even when the proxy is on your network. When a proxy sends
`X-Forwarded-For`, Trackside reads the client from it and ignores any `Forwarded` header beside
it, which the proxy may have passed through from the client untouched; `Forwarded` (RFC 7239)
is used only from a proxy that sends it alone.

```toml
[server]
trusted_proxies = ["127.0.0.1/32", "::1/128"]   # or the proxy container's network, e.g. "172.18.0.0/16"
public_urls = ["https://music.example.com"]
```

Things the proxy must do: pass `/api/v1/ping` through with its query string, unchanged and
uncached. Tracks asks it to prove the address is your server before it sends any credential,
and an address where that fails is never used.

Things the proxy must not do:

- buffer or compress `/api/v1/events` (server-sent events). Trackside sends
  `X-Accel-Buffering: no` and a keep-alive comment every 20 seconds, but some proxies need
  telling;
- time out long requests. A song download or a slow phone on cellular can take minutes;
- strip `Range`, `If-Range`, `If-None-Match` or `ETag`. Seeking and resumed downloads depend
  on them;
- cap upload size below `uploads.max_file` if uploads are on.

## Caddy

```caddy
music.example.com {
	reverse_proxy 127.0.0.1:4747 {
		flush_interval -1
	}
}
```

Trackside compresses its own responses, so Caddy doesn't need `encode`, and Caddy has no read
or write timeout on proxied requests by default. `flush_interval -1` sends bytes on as they
arrive, which keeps live updates and growing streams moving.

Caddy sets `X-Forwarded-For`, `-Proto` and `-Host` by itself. If Caddy runs in Docker
next to Trackside, use `reverse_proxy trackside:4747` and put both on the same network.

## nginx

```nginx
server {
    listen 443 ssl;
    http2 on;
    server_name music.example.com;

    ssl_certificate     /etc/letsencrypt/live/music.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/music.example.com/privkey.pem;

    client_max_body_size 2g;

    location / {
        proxy_pass http://127.0.0.1:4747;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header X-Forwarded-Host $host;
        proxy_buffering off;
        proxy_request_buffering off;
        proxy_read_timeout 1h;
        proxy_send_timeout 1h;
    }

    location /api/v1/events {
        proxy_pass http://127.0.0.1:4747;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header X-Forwarded-Host $host;
        proxy_set_header Connection "";
        proxy_buffering off;
        proxy_cache off;
        gzip off;
        proxy_read_timeout 1d;
    }
}
```

## Under a path

To serve Trackside at `https://example.com/music/` instead of its own name, set
`server.base_path = "/music"` and proxy that prefix without stripping it:

```caddy
example.com {
	handle /music/* {
		reverse_proxy 127.0.0.1:4747 {
			flush_interval -1
		}
	}
}
```

Add `https://example.com/music` as the address for Tracks.

## Checking it

```sh
curl -sI https://music.example.com/healthz          # 200
curl -s  https://music.example.com/api/v1/ping       # your server_id
curl -s 'https://music.example.com/api/v1/ping?challenge=0123456789abcdef&host=music.example.com&device=d0000000000000000000'
                                                      # same answer: the query reached the server
curl -sN https://music.example.com/api/v1/events -H "Authorization: Bearer tsk_..."
```

The last one should print a comment line (starting with `:`) every 20 seconds. If they arrive in bursts, the
proxy is buffering.
