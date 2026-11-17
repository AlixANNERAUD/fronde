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
When one of its turns ends, its result **wakes the parent**: no call has to wait or poll for it
(see [Wakes](#wakes) below). The parent can also read it with `threads_read`: its messages, its
answers and each turn's end, paged. Its messages include any you write yourself in the
sub-agent's thread from the chat: the parent agent reads them too.

When the turn needs the answer now, the call waits (`wait: true`). It returns:

- the sub-agent's final answer, the text of its turn after its message, when the turn ends;
- `waiting_for_user` as soon as a card opens in the sub-agent's thread: a permission or a
  question there is for you to answer, in that thread;
- `queued` as soon as the sub-agent waits for another agent to stop before it can start;
- `running` once `timeout_secs` passes, when the call is given up, or when the parent's own turn
  ends. The sub-agent goes on: a wait never stops it.
- `ended` when the turn had already ended before the call began to wait, and its result went to
  the parent as a wake: the result is not given a second time.

A result reaches the parent once: either a waiting call takes it, or it wakes the parent, never
both. The turn's end decides which, in one step with the calls registering to wait, so that a
call that starts waiting just as the turn ends either takes the result or learns that it went to
a wake.

What a sub-agent writes may come from anything it read, the web among it. It reaches its parent
capped, with hidden characters shown, and marked as data, not instructions.

## Wakes

When a sub-agent's turn ends and no call waits for it, Fronde writes a *sub-agent ended* entry in
its parent's thread (the sub-agent, how its turn ended, and its final answer: at most 8000
characters, hidden characters shown), then wakes the parent: a new turn of the parent's agent
whose prompt says which sub-agent ended and carries its answer between two marker lines, as data
the agent must never follow as instructions. The chat shows the entry as a small *Sous-agent
terminé* line linking to the sub-agent's thread; the answer itself is the agent's to read and
summarise.

- **One at a time.** A wake is a turn like any other. While the parent is in a turn, the wake
  waits for it to end, then runs; several wakes run one after the other, in the order the
  sub-agents ended. While a wake runs, a message you send is refused as during any turn. A wake
  takes an agent slot like any turn, so it may wait for one (*En file d'attente*).
- **Stop.** Stop in the chat ends the parent's running turn, a wake's included, and nothing else:
  its sub-agents go on, and one that ends afterwards still wakes it.
- **Archive.** An archived thread is not woken: a result logs nothing in it, and the wakes waiting
  when it was archived end at once, without reaching its agent. Its sub-agents' own threads keep
  their answers.
- **Restarts.** The parent's thread is the queue: the entry is written before the wake runs, and
  the wake writes a mark just before its prompt goes to the agent. A wake whose prompt never went
  out (Fronde stopped before it did) runs at the next start, once; one whose prompt went out is
  not given again: its turn ends as interrupted, like a message of yours a stop cut, and the
  wakes after it run in order. So a result reaches the parent at most once, and exactly once
  unless Fronde stops between that mark and the prompt.
- **Who is woken.** Only the thread whose agent started the sub-agent. An outside MCP client never
  is: it reads or waits.

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
  sub-agents' turns; what it wrote stays, and its parent is woken with how it ended. Stop in the
  chat stops only the thread you are in.

Sub-agents started per day, time per turn, configurable limits and depth, and archiving a thread
stopping its sub-agents are **planned**.

## Compared with T3 Code

Fronde follows T3 Code's delegation: a sub-agent gets only its task, works on its own by default,
wakes its parent with its result when it ends, can be waited for with a timeout that never stops it, ends a wait when it asks a question, can be
retried safely with a request id, and is stopped with its own sub-agents. It differs where Fronde
is not a coding tool: each sub-agent has its own confined directories rather than a shared or new
git checkout, there is no mode to raise (permissions are cards), the providers and models come
from `threads_list` rather than a tool of their own, and Claude Code's own sub-agents (`Task`)
stay off, so that every sub-agent is a Fronde thread you can see, confine and stop.
