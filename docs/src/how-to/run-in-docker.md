# Run in Docker

The image holds Fronde, its web interface, Node.js and the locked Claude adapter. See
[Container image](../reference/container-image.md) for what is inside.

## 1. Build the image

From a checkout:

```sh
docker build -t fronde .
```

The build installs the toolchain pinned in `mise.toml` and runs `mise run build`, as CI does.

## 2. Prepare the configuration and the secrets

In the container, Fronde must listen on every interface and keep its data on the volume:

```toml
[server]
listen = "0.0.0.0:8484"
token = { file = "/run/secrets/fronde_token" }

[data]
dir = "/data"
timezone = "Europe/Paris"
user = "alex"

[web]
password = { file = "/run/secrets/fronde_password" }
```

Create the secrets. Fronde runs as uid 65532, which must be able to read them:

```sh
(umask 077 && openssl rand -hex 32 > fronde_token)
(umask 077 && printf '%s' 'choose a password' > fronde_password)
sudo chown 65532 fronde_token fronde_password
```

## 3. Run it

```sh
docker run -d --name fronde -p 8484:8484 \
  -v ./fronde.toml:/config/fronde.toml:ro \
  -v ./fronde_token:/run/secrets/fronde_token:ro \
  -v ./fronde_password:/run/secrets/fronde_password:ro \
  -v fronde-data:/data \
  -e CLAUDE_CODE_OAUTH_TOKEN \
  fronde
```

`-e CLAUDE_CODE_OAUTH_TOKEN` passes the variable from your shell without writing it on the command
line. [Sign Fronde in to Claude](sign-in-to-claude.md) says how to get it.

The `fronde-data` volume keeps the memory, the conversation and the agent's home, where Claude
keeps its settings. Back it up as a whole.

## 4. Check it

```sh
docker ps          # STATUS shows (healthy) once /health answers
docker logs fronde
```

Then open <http://127.0.0.1:8484>. To reach it from other devices, put it
[behind a reverse proxy](reverse-proxy.md) with https.

## Docker Compose

The same, as a `compose.yaml`:

```yaml
services:
  fronde:
    image: fronde
    ports: ["8484:8484"]
    environment:
      - CLAUDE_CODE_OAUTH_TOKEN
    volumes:
      - ./fronde.toml:/config/fronde.toml:ro
      - ./fronde_token:/run/secrets/fronde_token:ro
      - ./fronde_password:/run/secrets/fronde_password:ro
      - fronde-data:/data
    restart: unless-stopped
volumes:
  fronde-data:
```
