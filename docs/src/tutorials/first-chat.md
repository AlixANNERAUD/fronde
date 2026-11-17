# Your first chat

This tutorial builds Fronde from source, starts it on your machine with the smallest useful
configuration, and has a first conversation with it. It takes about fifteen minutes, most of it
spent compiling.

## What you need

- Linux or macOS with `git`.
- [mise](https://mise.jdx.dev), which installs everything else at pinned versions: Rust, Node.js,
  the Dioxus CLI and Tailwind.
- A Claude subscription and [Claude Code](https://docs.anthropic.com/en/docs/claude-code)
  installed: `claude` must work in your terminal. Fronde's agent signs in once with it, in a home
  of its own (step 5).

## 1. Build Fronde

```sh
git clone https://github.com/AlixANNERAUD/fronde && cd fronde
mise install
mise run build
```

`mise run build` builds the web interface into `target/dx/fronde-web/release/web/public`, then
the server into `target/release/fronde`.

## 2. Install the Claude adapter

```sh
mise run agent
```

This installs the Claude ACP adapter at the version locked in `agent/package-lock.json` into
`agent/node_modules`. Inside the checkout, mise puts `agent/node_modules/.bin` on the `PATH`, so
Fronde finds `claude-agent-acp` there.

## 3. Write a configuration

```sh
target/release/fronde init
```

This writes `fronde.toml`, `fronde_token` and `fronde_password` in the current directory, all
mode 0600, using your user name and system time zone. It prints a chat password once (it is also
in `fronde_password`): note it. It refuses to overwrite existing files unless you pass `--force`.
Because you ran it in the checkout, it also wrote the full path of the adapter installed in step
2 into `[agent] command`. Check the setup:

```sh
target/release/fronde check --config fronde.toml
```

It prints `config OK`, or says what is wrong. Fronde keeps its data in
`~/.local/share/fronde` (or `$XDG_DATA_HOME/fronde`); `check` creates the directory and proves it
is writable.

## 4. Start Fronde

```sh
mise exec -- target/release/fronde serve --config fronde.toml
```

The log says `listening on http://127.0.0.1:8484 (MCP at /mcp)`. Keep it running, and run it
from the checkout: that is where it finds the interface's files. `mise exec` puts the adapter on
the `PATH`, in case `init` did not find it.

## 5. Sign the agent in to Claude

The agent has a home of its own in the data directory and does not use your `~/.claude`. It signs
in with a long-lived token. In another terminal, in your own home as usual, run:

```sh
claude setup-token
```

Follow the sign-in in your browser; `claude` prints the token. Save it in a file only you can read,
beside `fronde.toml`:

```sh
(umask 077; cat > claude-token)   # paste the token, press Enter, then Ctrl-D
```

Add it to `fronde.toml`, then restart `fronde serve`:

```toml
[agent]
oauth_token = { file = "claude-token" }
```

Never run `claude` with `HOME` set to the agent's home to sign it in: the agent can write that
home, and what it left there would run with your rights.

## 6. Sign in and chat

Open <http://127.0.0.1:8484>. You land on the sign-in page (*Connexion*). Type the password
`fronde init` printed and press *Se connecter*.

Write a first message, for example *Retiens que je préfère le thé au café*, and send it. The
first message starts the agent, which takes a few seconds. Then the answer streams in, and the
tool Fronde uses to save the fact shows as a card while it runs. Press *Stop* to end a turn
early. Should the agent ever want a tool it was not given up front, a card *Fronde demande
l'autorisation* shows what it would do, with *Autoriser une fois* and *Refuser*; left unanswered,
it refuses after 30 minutes.

Now look at `~/.local/share/fronde/memory/MEMORY.md`: the fact is there, as one line. Ask *Qu'est-ce que je
préfère boire ?*, now or after a restart: the hub hands its core memory to every agent that
connects to it.

When a choice is yours, Fronde asks instead of guessing. Try *Propose-moi un thé pour ce soir,
demande-moi d'abord ce que j'aime*: a card titled *Question de Fronde* shows options, the advised
one marked *Recommandé*, and *Autre…* for an answer in your own words. Pick one and press
*Envoyer*, or *Refuser* to let Fronde go on without it. The card then shows your answer, and
Fronde carries on from it. The card stays answerable from any page where you are signed in, your
phone included, until it is answered or expires.

## What you built

A Fronde server with its memory in `~/.local/share/fronde`, a chat behind a password, and an MCP server at
`/mcp` behind the token. Next steps:

- [Connect an agent over MCP](../how-to/connect-an-agent-over-mcp.md) so Claude Code in your
  terminal shares the same memory.
- [Configure the calendar](../how-to/configure-calendar.md).
- [Run in Docker](../how-to/run-in-docker.md) to keep it running on a server.
