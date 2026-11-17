# Threads and sub-agents

A **thread** is one conversation: its own stream of messages, its own agent session, and its own
agent process with its own directories, sandbox and hub token (see
[How Fronde works](architecture.md#threads) and [Security model](security.md#threads)). The chat
is the main thread; you start others from the chat.

A thread's agent can also start threads of its own for a task: **sub-agents**. Fronde's agent
hands a long or separate piece of work to a sub-agent, and goes on with the conversation, or waits
for the answer when it needs it now. The tools are `threads_start`, `threads_send`,
`threads_read`, `threads_list` and `threads_cancel` (see [Tools](../reference/tools.md#threads)).

## What a sub-agent is

A sub-agent is a thread like any other: a row in the `threads` table with its parent, a depth one
below its parent's, and `started_by` naming the thread that started it. It belongs to its
parent's owner. It runs in its own agent process, in its own directories
(`<data>/threads/<id>/work` and `home`), confined like every agent, and takes a slot of the
agent pool among those kept for threads you did not start, so that a thread you talk to always
gets one.

## What a sub-agent sees

Only its task. Its first message is the task, marked as written by its parent, in a new session
(never a resume of its parent's), with a system prompt of its own: work alone, and end with a
short answer that stands on its own. It sees neither its parent's conversation nor its files, and
a secret its parent was given "for this conversation" does not work in the sub-agent's thread.
So the task must say everything the sub-agent needs, and never a password, token or key.

It runs on its parent's provider unless the call names another (only Claude is configured today),
and on the model the call names, among those the provider's agent offered when it last started,
or else on the provider's default.

## How the answer comes back

By default the call that starts a sub-agent returns at once, and the sub-agent works on its own.
The parent reads it with `threads_read`: its messages, its answers and each turn's end, paged.
Its messages include any you write yourself in the sub-agent's thread from the chat: the parent
agent reads them too.

When the turn needs the answer now, the call waits (`wait: true`). It returns:

- the sub-agent's final answer, the text of its turn after its message, when the turn ends;
- `waiting_for_user` as soon as a card opens in the sub-agent's thread: a permission or a
  question there is for you to answer, in that thread;
- `queued` as soon as the sub-agent waits for another agent to stop before it can start;
- `running` once `timeout_secs` passes, when the call is given up, or when the parent's own turn
  ends. The sub-agent goes on: a wait never stops it.

A turn's end hands its result to the calls waiting for it, and an answer a waiting call took is
marked delivered; the hand-over and the mark happen together, so that a result is either taken by
a waiting call or left for the parent to be told otherwise. Waking the parent with its sub-agent's
answer when it ends, so that no call has to wait, is **planned**.

What a sub-agent writes may come from anything it read, the web among it. It reaches its parent
capped, with hidden characters shown, and marked as data, not instructions.

## What a thread may reach

A thread's agent is known to the hub by its token alone. It may read, send to and stop only the
threads it started, and theirs; any other thread, its own parent and its siblings among them, is
refused exactly as a thread that does not exist, and its own thread is refused too. It may do so
only during one of its turns: its token lives while its agent idles, and a process the agent
started holds it too, but neither has a turn. An outside MCP client gets no `threads_*` tools
(letting one start threads of its own is **planned**).

## Limits

- **Depth.** The threads you start may start sub-agents; a sub-agent may not: it is not offered
  `threads_start` and `threads_send`, and the hub refuses them if it calls them anyway.
- **Agents at once.** Sub-agents share the pool's slots but one with every other thread you did
  not start (`[threads] max_running`).
- **Per parent.** A thread has at most 3 sub-agents in a turn at once (waiting for an agent
  included) and keeps at most 30 sub-agents not archived.
- **Retries.** A call retried in the same turn with the same `client_request_id` joins the
  sub-agent the first one started, so that a lost reply does not start two.
- **Stopping.** `threads_cancel` stops a sub-agent's running turn, its pending cards and its own
  sub-agents' turns; what it wrote stays. Stop in the chat stops only the thread you are in.

Sub-agents started per day, time per turn, configurable limits and depth, and archiving a thread
stopping its sub-agents are **planned**.

## Compared with T3 Code

Fronde follows T3 Code's delegation: a sub-agent gets only its task, works on its own by default,
can be waited for with a timeout that never stops it, ends a wait when it asks a question, can be
retried safely with a request id, and is stopped with its own sub-agents. It differs where Fronde
is not a coding tool: each sub-agent has its own confined directories rather than a shared or new
git checkout, there is no mode to raise (permissions are cards), the providers and models come
from `threads_list` rather than a tool of their own, and Claude Code's own sub-agents (`Task`)
stay off, so that every sub-agent is a Fronde thread you can see, confine and stop.
