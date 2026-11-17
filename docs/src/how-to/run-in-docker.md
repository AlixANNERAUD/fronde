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

If you use Docker's own `secrets:` (Compose or Swarm) instead of bind-mounted files, the runtime
mounts them with mode 0444 and root as owner, which Fronde refuses by default. Set
`FRONDE_ALLOW_SHARED_SECRET_FILES=1` in the container's environment to accept them. Bind-mounted
files kept at mode 0600 and owned by uid 65532, as above, need no override.

## 3. Run it

```sh
docker run -d --name fronde -p 8484:8484 --ulimit core=0 \
  -v ./fronde.toml:/config/fronde.toml:ro \
  -v ./fronde_token:/run/secrets/fronde_token:ro \
  -v ./fronde_password:/run/secrets/fronde_password:ro \
  -v fronde-data:/data \
  -e CLAUDE_CODE_OAUTH_TOKEN \
  fronde
```

`-e CLAUDE_CODE_OAUTH_TOKEN` passes the variable from your shell without writing it on the command
line. `--ulimit core=0` keeps a crash from writing a core dump with secrets in it; Fronde also
turns core dumps off for itself at start. [Sign Fronde in to Claude](sign-in-to-claude.md) says how to get it.

The `fronde-data` volume keeps the memory, the conversation and the agent's home, where Claude
keeps its settings. Back it up as a whole. It never holds the master key of
[stored secrets](#stored-secrets), which is kept apart.

## The agent's sandbox

Fronde confines its agent with Landlock and a seccomp filter of its own (see
[Where the agent runs](../explanation/security.md#where-the-agent-runs)), which the host's kernel
provides: a container cannot bring its own. Docker's default seccomp profile allows both. Check
that the host enables Landlock:

```sh
cat /sys/kernel/security/lsm   # the list must include landlock
```

Debian 12 and later, Ubuntu 22.04 and later, Fedora and Arch enable it. Fronde also needs Linux
6.2 or later (Landlock ABI 3), which Debian 12's own kernel (6.1) is not: on such a host, install
a newer kernel (`bookworm-backports` has one), or set `[agent] sandbox = false`. Ubuntu 22.04
needs its HWE kernel for the same reason. `docker logs fronde` shows an `agent sandbox:` line at
start that says what the kernel enforces. Without it, the agent does not start; set
`[agent] sandbox = false` only if you accept an unconfined agent. In the
container, the agent's home is `/data/home` and no host socket (D-Bus, the user manager, the
Docker socket) is there unless you mount it: do not mount one.

## Limits

A container has no systemd user manager, so [`[agent.limits]`](../reference/configuration.md#agentlimits)
does not apply there: `docker logs fronde` shows `agent limits: none (no systemd-run here)`.
Cap the container instead, which holds Fronde and its agent:

```sh
docker run … --memory 3g --cpus 2 --pids-limit 1024 …
```

(`mem_limit`, `cpus` and `pids_limit` in Compose). Each thread has its own agent, and at most
[`[threads] max_running`](../reference/configuration.md#threads) of them run at once (3 by
default; one idle for `idle_stop`, 10 minutes, stops): without systemd, that number and the
container's `--memory` are the agents' whole budget, and `[agent.limits] total_memory` does not
apply. Count about 0.5 GiB for each agent with its Claude Code, plus Fronde itself, when you
pick `--memory`; lower `max_running` on a small machine. When the container runs out of memory, the
kernel kills the agent first: Fronde starts it with the highest `oom_score_adj` (1000), and
the chat starts a new one at the next message. Fronde sets no `ulimit` of its own besides core
dumps (none for the agent): Linux counts processes per user, not per container, and the
agent's Node.js reserves far more address space than it uses, so neither `RLIMIT_NPROC` nor
`RLIMIT_AS` caps it well; `--pids-limit` and `--memory` do.

## 4. Check it

```sh
docker ps          # STATUS shows (healthy) once /health answers
docker logs fronde
```

Then open <http://127.0.0.1:8484>. To reach it from other devices, put it
[behind a reverse proxy](reverse-proxy.md) with https.

## Stored secrets

To use [`fronde secret`](store-a-secret.md), Fronde needs a place for its master key outside the
data volume, so that a backup or a snapshot of the volume does not hold the key with the secrets
it opens. By default the key goes beside the configuration, but `/config` is read-only to
Fronde here (the image's directory belongs to root, and the file is mounted `:ro`), so
`fronde secret set` would stop with an error naming the fix. Give the key a directory of its
own, writable by uid 65532:

```sh
mkdir -m 700 fronde-keys && sudo chown 65532 fronde-keys
```

name the file in the configuration:

```toml
[secrets]
backend = "file"
master_key = { file = "/keys/master.key" }
```

(`backend = "file"` says what `auto` would find anyway in a container, which has no keychain and
no systemd credential.)

and add `-v ./fronde-keys:/keys` to `docker run` (or `- ./fronde-keys:/keys` to the Compose
volumes). Do not mount the configuration's directory writable instead: the agent runs as the
same user and could then change the configuration, component hashes included. Then store a
secret through the running container, the value on standard input:

```sh
docker exec -i fronde /app/fronde secret set caldav --origin https://cloud.example \
  --config /config/fronde.toml < caldav-password
```

The first secret creates `fronde-keys/master.key`. Back it up once, apart from the volume's
backups.

## Docker Compose

The same, as a `compose.yaml`:

```yaml
services:
  fronde:
    image: fronde
    ports: ["8484:8484"]
    ulimits:
      core: 0
    mem_limit: 3g
    cpus: 2
    pids_limit: 1024
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
