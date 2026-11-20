# Why subscriptions and ACP

Fronde uses the Claude subscription you already pay for, through the same Claude Code you use in a
terminal. It does not call the Anthropic API with a key.

## Why not an API key

- **Cost.** An assistant that reads your calendar and memory on every message is cheap on a
  subscription you already have, and adds up quickly when billed per token.
- **One account.** Your usage stays in one place, under the limits you know.
- **No key to guard.** Fronde never stores a model provider's secret. The sign-in belongs to
  Claude Code, which keeps it in `~/.claude` or receives it as `CLAUDE_CODE_OAUTH_TOKEN`.

## Why ACP

The [Agent Client Protocol](https://agentclientprotocol.com) is how editors drive coding agents:
a JSON-RPC session over standard input and output, with streamed text, tool calls and permission
requests. Claude Code speaks it through its
[ACP adapter](https://github.com/agentclientprotocol/claude-agent-acp). Driving the agent this way
means:

- Fronde runs the official client on its official sign-in, not an imitation of it.
- Fronde gets the agent's own loop: tool use, context management, web search, resumable sessions.
- The same client code can drive other agents that speak ACP. Codex and Antigravity are
  **planned** (see [Other providers](#other-providers)).

The adapter is locked in `agent/package-lock.json` and installed with `mise run agent`, or into
the container image, so an update of the adapter is a reviewed change.

## Other providers

What is Claude's alone is gathered in a provider profile (see
[How Fronde works](architecture.md#providers-and-models)); Claude is the only provider, and
`[agent]` holds its settings. Another provider needs, besides its profile, a sign-in design of
its own: each of these CLIs keeps its sign-in in a home it writes during an interactive login,
and Fronde never runs a CLI in a home its agent can write (see
[Sign Fronde in to Claude](../how-to/sign-in-to-claude.md)). Each comes with its own
`[providers.<id>]` table.

- **Codex**, **planned**, through `@agentclientprotocol/codex-acp` (Apache-2.0, an npm package
  like Claude's adapter, so its sandbox grant is its `node_modules`). It needs: a sign-in other
  than an API key (the ChatGPT login writes `~/.codex`); a way to carry the hub's token, since
  custom headers on its MCP servers are not documented and it may not expand `${VAR}` (a
  `bearer_token_env_var` setting would keep the token off its command line); its permission
  option ids read and listed before any lasting option is offered (until then none is); its own
  `_meta`, if any, to turn off the owner's configuration; and its environment variables kept by
  the sandbox. It lists its models, reasoning effort, approval and sandbox as config options.
- **Gemini** means **Antigravity** (`agy acp`), **planned**: on a personal subscription, Gemini
  CLI was replaced by Antigravity in June 2026. It is a closed-source native binary (its sandbox
  grant is its directory). Its sign-in is a cached interactive login (`agy acp --login`); whether
  it takes MCP servers from the client, with which headers, and how it lists models, is not
  documented and must be checked first. Third-party bridges warn of a terms-of-service risk and
  are not used.

Until a provider's permission options are read, its profile offers no lasting option and none on
a mode switch: a new provider never inherits Claude's lists.

## Usage and quotas

A subscription has two usage windows: one of 5 hours and one of a week. Fronde reads how much
of each is used from what the Claude adapter already sends during a turn, and keeps the latest
reading per subscription account in `fronde.db`, so that it survives a restart. It makes no request of its
own for it: no call to an Anthropic endpoint, no `/usage` turn.

What the adapter sends:

- A **reading only when the limits change.** Claude Code emits a `rate_limit_event` when an API
  response's rate-limit headers change (about once per percentage point); the adapter passes it
  on in `_meta["_claude/rateLimit"]` of the next `usage_update`, maybe a later turn's. A turn
  whose limits did not change brings no reading.
- **One window, documented**: its kind (`five_hour`, `seven_day`, `seven_day_opus`,
  `seven_day_sonnet`, `overage`, …), its status (`allowed`, `allowed_warning`, `rejected`), its
  reset time, and its use as a fraction from 0 to 1, each of them optional.
- **Both windows, undocumented**: Claude Code adds `unifiedWindows`, the 5-hour and weekly
  windows at once. It is marked internal and may change with any release: Fronde reads it when
  it is there and falls back on the documented window.
- **Paid extra usage in use** (`isUsingOverage`), kept as an extra window marked as a warning.
- **A turn refused for a limit**: the prompt fails with `errorKind: "rate_limit"` and the latest
  reading, whose reset time says when turns run again. The turn ends with
  « Limite d'usage atteinte, reprise vers 16:40. » (the day added when it is a day or more away)
  and the reading is kept.

What it does not send, and Fronde does not make up:

- **Nothing between turns.** Your use elsewhere (a terminal, claude.ai) shows only at Fronde's
  next turn; each window carries the time it was read.
- **No reading before Fronde's first turn**, and no plan, account or window length: the length
  comes from the kind.
- **Per-model weekly windows** only when one is the event's own window.

Every field is checked and anything odd is left out: a use outside 0 to 1 by more than a few
points (`42` would be a percentage) is dropped rather than rescaled, a value just past an edge is
clamped, a window whose reset time does not read or lies further ahead than the window lasts
(5 hours for the 5-hour window, 7 days for a weekly one, 31 days otherwise, each with 10 minutes
to spare; milliseconds, say) is refused whole, and a reading that does not parse never fails a
turn. The log names the fields it left out, never the whole `_meta`. Codex and Antigravity send
no reading Fronde reads yet.

Readings from several agent processes may arrive out of order. A window is replaced by one with a
later reset time (it started again); for the same reset time, Fronde keeps the greater use and
the worse status, so a window's use only goes down at its reset and a reading held back elsewhere
never lifts a pause. Once a window's reset time has passed, it reads as reset since the reading,
its use unknown (shown so, never as 0 %) until the next reading. Windows of kinds Fronde has no
name for are kept, four at most, and dropped when a reading of both the 5-hour and the weekly
window no longer lists them.

### One subscription per person

Readings belong to a subscription account, the one the agent that sent them signs in with: its
thread's (see [`[accounts.<name>]`](../reference/configuration.md#accountsname)), kept
under that account's name. A reading from an agent without an account (its owner has none, or
is no longer a user, so it never starts) is kept nowhere: never under another account, never
under one name shared by everyone without one.

Readings were first kept under the provider's name, then under each account's name in the same
place. Fronde moves such a reading to its account at the next start (the legacy form's account
is named `claude`, as its readings were), and `fronde check` reads it where it is until then, so
that a pause it holds is not lost at an upgrade; when the account already has a reading, the
later one is kept. A reading kept there under a name no account has is removed at start, never
given to another account: a new account's first reading comes with its next turn, the daily
counts deciding until then.

Fronde keeps a share of each window for the account's owner: past `[threads] reserve` (30 % by
default) of an account, sub-agents on that account are neither started nor sent messages, and
their results wait for its owner's next message (see
[What background work may spend](threads.md#what-background-work-may-spend)). The line is each
account's: your windows past it stop your background work, never a family member's, and hers
past it never stop yours. The daily counts that stand in when no reading decides are each
account's too.

The chat's header shows each person their own account's windows as a meter, and an admin also
every other account's, by name (see
[Use several conversations](../how-to/use-threads.md#see-how-much-of-your-subscription-is-used));
`fronde check` and the start log print one `usage:` line per account (see
[Command line](../reference/command-line.md)).

### One subscription, several people

An account may list other people in `users`, and a person may use several accounts of one
provider: their own Claude and a family one, say (see
[`[accounts.<name>]`](../reference/configuration.md#accountsname)). Each person has one default
account per provider, named by `default_for` when they may use more than one, so that who pays
for a conversation is always written in the file, never guessed. A conversation runs on its
owner's default, which may be an account someone else subscribes to, unless its owner picked
another one they may use for it; its sub-agents run on the same. Picking an account from the page
is **planned**. A pick is never replaced behind its owner's back: when they may no longer use it,
the conversation refuses messages until they pick another, rather than run on their default, which
may be a subscription kept for other work.

What the people of one account share is what the subscription itself counts:

- its **usage windows**: everyone's turns on it spend the same 5-hour and weekly windows;
- its **reserve**: `[threads] reserve` keeps a share of those windows for its owner, the
  subscriber, and everyone's background work on it stops at that line;
- its **daily counts** of sub-agents and their turns, all its users together.

What they never share:

- **conversations**: a thread is its owner's, whatever accounts two people have in common, and
  only they and an admin see it;
- **homes and working directories**: each thread keeps its own, and its sandbox writes only
  those;
- **sessions**: no transcript and no `~/.claude` state goes from one person's thread to another's;
- **credentials**: the token reaches each agent through its environment only, and Fronde writes no
  credential file. Two accounts never read one token either: sharing goes through `users`.

The terms of a personal subscription probably do not allow sharing it with other people: a
Claude Pro or Max plan is meant for one person. Check your provider's terms before you list
anyone in `users`; Fronde makes sharing possible and keeps it visible, it does not make it
allowed.

## Why MCP for the tools

Fronde's tools are not wired into one agent: the hub serves them over MCP, the protocol every
agent already speaks. Fronde's own agent reaches them like any other client, at `/mcp` with the
token. The memory and calendar you use in the chat are the same ones Claude Code sees in your
terminal.

## The trade-off

A subscription is meant for a person using their own agent, and comes with usage limits. Fronde
is built for exactly that: one owner, one conversation, the owner's own machine. It is not a way
to serve other people from one subscription.
