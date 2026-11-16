# Introduction

Fronde is a small self-hosted personal assistant that runs on **your own Claude subscription**.
It gives you a web chat with Claude from any browser, and it keeps one set of tools for every
agent you use: a memory in Markdown files, your CalDAV calendar and reminders, and the tools of
other MCP servers.

Fronde does not call a model API with a paid key. It drives Claude Code through its
[ACP adapter](https://github.com/agentclientprotocol/claude-agent-acp), signed in to your
subscription, and serves its tools to it over MCP. Other agents, such as Claude Code in your
terminal, reach the same tools at `/mcp`.

## What it does

- **Chat** from a phone or a computer, behind a password. Answers stream in, the tools Fronde uses
  show as they run, any turn can be stopped, and the conversation survives restarts.
- **Remembers** what you tell it in plain Markdown files (`USER.md`, `MEMORY.md` and a dated
  journal), searchable and versioned with git.
- **Reads and writes your calendar and reminders** on any CalDAV server.
- **Brings in more tools** from upstream MCP servers, found on demand with `search_tools`.
- **Serves all of this over MCP** at `/mcp`, behind a bearer token, for your other agents.
- **Runs as one binary or one container**, configured with one TOML file. Secrets live in files
  or environment variables, never in the configuration.

The chat's interface is in French for now.

## How this guide is organised

The guide follows [Diátaxis](https://diataxis.fr):

- [Tutorials](tutorials/first-chat.md) take you from nothing to a first chat.
- [How-to guides](how-to/run-in-docker.md) answer one task each: Docker, signing in to Claude,
  connecting agents, adding MCP servers, the calendar, a reverse proxy, development.
- [Reference](reference/configuration.md) lists every setting, command, endpoint and tool.
- [Explanation](explanation/architecture.md) says how Fronde works and why it is built this way.

Pages say **planned** for what is designed but not built yet.

## The name

A *fronde* is a sling, the small one's weapon against giants, and *la Fronde* was the
17th-century French revolt against central power. Fronde runs at home and drives the big models
through your own subscriptions instead of locking you into their platforms. It is pronounced the
French way, /fʁɔ̃d/.
