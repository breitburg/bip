# Bip

Bip is an AI agent in development. It runs as a WebSocket server; any client
that speaks the protocol below can talk to it. `static/index.html` is a minimal
one, served by Bip itself.

## Running

Create a `.env` file:

```
OPENAI_API_KEY=...
OPENAI_BASE_URL=https://.../v1
```

The endpoint must support Chat Completions with streaming and tool calls.
Then:

```
cargo run
```

and open http://127.0.0.1:8765 in a browser. Set `RUST_LOG` (for example
`RUST_LOG=bip=debug`) to change how much is logged; the default is `info`.

## Protocol

Bip serves the files in `static/` over HTTP on `http://127.0.0.1:8765`, and
the protocol on the WebSocket endpoint `ws://127.0.0.1:8765/ws`. Every WebSocket message, in either
direction, is a JSON text frame of the form:

```json
{"type": "<type>", "data": <payload>}
```

The two directions have separate message types.

### Client → Bip

Clients control the whole swarm, not only `main`: `message` and `stop` name
the agent they're for in `agent`, and go to `main` when they name none.

| `type`    | `data` | Meaning                                    |
|-----------|--------|--------------------------------------------|
| `message` | `{"text", "agent"?}` | A message for that agent from the person at this client. |
| `stop`    | `{"agent"?}` or none | Stop what that agent is doing: its reply ends where it is, and every subagent it started is stopped, with no report. The context is kept, with what the agent had said so far. Messages that were waiting for the reply join the context unanswered; the agent answers them with the next message. A stopped subagent then finishes, and reports what it had said, marked `(stopped)`. |
| `clear`   | none   | Remove every message from `main`'s context, and stop and remove every subagent. It takes effect at once, even in the middle of a reply. |

```json
{"type": "message", "data": {"text": "Hey Bip!"}}
{"type": "message", "data": {"text": "Focus on the tests", "agent": "sub-1"}}
{"type": "stop"}
{"type": "stop", "data": {"agent": "sub-2"}}
{"type": "clear"}
```

A `stop` or a `clear` takes effect as soon as the agent is not inside a
script, so at most 10 seconds later. A command for an agent that doesn't
exist, or has finished, gets an `error` back, to the client that sent it only.

Each agent handles messages one at a time, in the order they arrive, across
all clients. A message that arrives while it's replying waits for the reply
to end.

### Bip → client

| `type`   | `data` | Meaning |
|----------|--------|---------|
| `change` | `{"agent", "patch"}` | A [JSON Patch](https://www.rfc-editor.org/rfc/rfc6902) to one agent's context. |
| `error`  | `{"agent", "message"}` | Something went wrong in that agent's turn. It keeps running. |
| `finished` | `{"agent"}` | That agent is gone, done or stopped. Nothing more will come from it. |
| `cleared` | none | A `clear` is complete: `main` is empty and every subagent is gone, so clients can forget their contexts. |

#### Agents

Bip has exactly one tool, `lua`, which runs Luau code. Each agent has its
own runtime, and globals persist between calls until a `clear`. The runtime
is sandboxed: only the `string`, `table`, `math`, `utf8`, `bit32` and
`buffer` libraries, no files, processes or network, 10 seconds per call and
64 MB in all.

Everything else Bip can do, it does from a script, through interfaces:
global tables of functions that reach outside the sandbox. The primary agent
has two:

- `context`: read its own conversation (`context.messages()`), and edit and
  rewind it (`context.revise{...}`).
- `agents`: `agents.spawn(task)` starts a subagent. It works on its task in
  the background, with its own context, and when it finishes its final reply
  is added to the primary agent's context as a report. The primary agent then
  responds to it. A subagent can run Luau too, but has no interfaces.
  `agents.stop(name)` stops one it started before it finishes; it then sends
  no report.

An interface is defined in Rust as a name, the documentation the model reads,
and a function per entry, each given the runtime, the calling agent and the
script's arguments (see `src/interface.rs`):

```rust
Interface::new("agents", DOCS).function("spawn", |_, agent, task: String| { ... })
```

Every output names the agent it belongs to: `main` for the primary agent, and
`sub-1`, `sub-2`, ... for subagents, in the order they started. Clients get
every agent's changes, so they can show what each one is doing. Each agent's
context is separate and starts empty. Clients can message and stop any agent
by name. Names are unique until a `clear`, which starts numbering over.
A subagent starts as soon as `agents.spawn` is called. When it finishes, or a `clear`,
a `stop` or `agents.stop` stops it, Bip sends `finished` for it: it's gone, and so is its context.

Bip doesn't send "replies". It streams changes to its context, the
conversation it sees, as JSON Patches. The system prompt (`system.md`) isn't
part of the context, so it never appears here. A new message, each streamed piece of
Bip's reply, and Bip editing its own history all arrive the same way. Bip only
uses the `add`, `remove` and `replace` operations.

The context starts as:

```json
{"messages": {}, "order": []}
```

Applying an agent's `change`s to it in order gives exactly that agent's
context.
`messages` maps each message's ID to the message, and `order` lists the IDs in
conversation order. Messages are keyed by ID so that inserting or removing one
doesn't resend the messages after it. No change ever carries the whole
conversation.

When a client connects, Bip first replays every output sent so far, so the
client can rebuild the context from the empty state above. After that, outputs
arrive live. The stream is always on: there's no "reply finished" message, and
Bip may change its context at any time, not only after a message.

Every connected client receives every output. The exception is an `error`
about invalid input, which goes only to the client that sent it. A client that
falls far behind (256 outputs) misses the ones it couldn't keep up with.

#### Messages

```json
{
  "id": "0E9258",
  "role": "assistant",
  "content": "Hey Ilia! What's up?",
  "time": "2026-11-22T01:47:53.964114Z",
  "tool_calls": [
    {"id": "call_1", "name": "lua", "arguments": "{\"code\": \"agents.spawn(...)\"}", "result": "sub-1"}
  ]
}
```

- `id`: 6 characters, unique within the context and stable for the message's
  life.
- `role`: `"user"`, `"assistant"`, or `{"agent": "sub-1"}` for a message from
  another agent: a subagent's report, or the task a subagent was given.
- `time`: when the message joined the context.
- `tool_calls`: tools the assistant called in this message; the only tool is
  `lua`. `arguments` is the JSON text the model wrote. `result` is `null` while the tool runs, then
  the text it returned. A result is part of its call, not a message of its
  own.

#### Example

Bip starts a reply, then streams its text:

```json
{"type": "change", "data": {"agent": "main", "patch": [
  {"op": "add", "path": "/messages/0E9258", "value": {"id": "0E9258", "role": "assistant", "content": "", "time": "...", "tool_calls": []}},
  {"op": "add", "path": "/order/2", "value": "0E9258"}
]}}
{"type": "change", "data": {"agent": "main", "patch": [{"op": "replace", "path": "/messages/0E9258/content", "value": "Hey Ilia"}]}}
{"type": "change", "data": {"agent": "main", "patch": [{"op": "replace", "path": "/messages/0E9258/content", "value": "Hey Ilia! What's up?"}]}}
```

JSON Patch has no operation for adding to a string, so each streamed piece
replaces the message's whole `content`.
