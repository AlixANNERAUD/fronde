# Run behind a reverse proxy

To use Fronde from a phone or from outside your home, serve it over https behind a reverse proxy.

## What the proxy must do

- **Forward the `Host` header unchanged**, port included. The chat's WebSocket (`/ws`) and the
  sign-in and sign-out forms are only taken when the page's `Origin` names the same host as the
  `Host` the server receives, and, with `secure_cookie = true`, when the page is https. A proxy
  that rewrites `Host` to `127.0.0.1:8484`, or drops a port other than 443, gets
  `403 Forbidden` on `/ws` and on signing in.
- **Pass WebSocket upgrades through** on `/ws`, and keep them open: a turn can stream for minutes.
- **Serve https**, then set `secure_cookie = true` under `[web]` so the session cookie is only
  sent over https.

```toml
[web]
password = { file = "/run/secrets/fronde_password" }
secure_cookie = true
```

## Caddy

Caddy does all of this by default:

```text
fronde.example.org {
    reverse_proxy 127.0.0.1:8484
}
```

## nginx

```nginx
map $http_upgrade $connection_upgrade {
    default upgrade;
    ''      close;
}

server {
    # listen, server_name and certificates as usual
    location / {
        proxy_pass http://127.0.0.1:8484;
        proxy_http_version 1.1;
        # $http_host is the Host the browser sent, port included; $host drops the port.
        proxy_set_header Host $http_host;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection $connection_upgrade;
        proxy_read_timeout 1h;
    }
}
```

## Agents through the proxy

`/mcp` goes through the same proxy. Agents on other machines use
`https://fronde.example.org/mcp` with the bearer token. See
[Connect an agent over MCP](connect-an-agent-over-mcp.md).
