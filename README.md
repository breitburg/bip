# Bip

Bip is a foundation for asynchronous AI agents.

Each agent has one tool, a sandboxed Luau runtime. Scripts call interfaces to
reach external systems, spawn subagents and edit the agent's own context.
Adding a capability means adding an interface.

Agents run in the background and accept messages while they work. Every
change to an agent's context is streamed to clients as it happens. The
protocol works over any transport, and WebSocket is the one implemented so
far. `static/index.html` is a playground for testing.

## Running

Put the model endpoint in `.env`. It must support Chat Completions with
streaming and tool calls.

```
OPENAI_API_KEY=...
OPENAI_BASE_URL=https://.../v1
```

Then run `cargo run` and open http://127.0.0.1:8765. `RUST_LOG` sets the log
level, for example `RUST_LOG=bip=debug`. The default is `info`.

## Agents

The primary agent is called `main` and always exists. Subagents get a
6-character ID like `4C1D7A`. No two running agents share a name. Each agent
has its own context, which starts empty. The system prompt in `system.md` is
sent with every request but isn't part of the context.

The only tool is `lua`. Each agent gets its own Luau runtime, and globals
persist between calls until a reset. The sandbox has the `string`, `table`,
`math`, `utf8`, `bit32` and `buffer` libraries and nothing else. There's no
file, process or network access. A call may run for 10 seconds, and an
agent's runtime may use 64 MB.

Interfaces are global tables of Rust functions that scripts can call. `main`
has two of them.

- `context` reads the agent's own conversation with `context.messages()`, and
  edits or rewinds it with `context.revise{...}`.
- `agents.spawn(task)` starts a subagent in the background and returns its
  name. When the subagent finishes, its last reply arrives in the parent's
  context as a message from it, and the parent responds. `agents.stop(name)`
  stops a subagent early, and then it sends no report.

Subagents can run Luau but have no interfaces.

An interface is a name, docs for the model, and a function per entry. Each
function gets the runtime, the calling agent and the script's arguments. See
`src/interface.rs`.

```rust
Interface::new("agents", DOCS).function("spawn", |_, agent, task: String| { ... })
```

## Protocol

Every message in either direction is JSON.

```json
{"type": "<type>", "data": <payload>}
```

Over WebSocket, the endpoint is `ws://127.0.0.1:8765/ws` and each message is
a text frame. The same server serves `static/` over HTTP.

### Commands

`message` and `stop` take an optional `agent`. Without one they go to `main`.

| `type`    | `data` | Effect |
|-----------|--------|--------|
| `message` | `{"text", "agent"?}` | Sends the agent a message from the person at this client. |
| `stop`    | `{"agent"?}` or none | Ends the agent's reply where it is and kills its subagents without reports. The context is kept. Messages queued during the reply are added but not answered until the next message. A stopped subagent finishes and reports what it had said, marked `(stopped)`. |
| `reset`   | none   | Empties `main`'s context, drops its Lua runtime, and kills every subagent along with their runtimes. |

```json
{"type": "message", "data": {"text": "Hey Bip!"}}
{"type": "message", "data": {"text": "Focus on the tests", "agent": "4C1D7A"}}
{"type": "stop"}
{"type": "stop", "data": {"agent": "0B3E95"}}
{"type": "reset"}
```

`stop` and `reset` interrupt a reply, but wait for a running script to end,
which takes at most 10 seconds. A command for an agent that doesn't exist
gets an `error` back, sent only to that client.

Each agent handles messages one at a time, in arrival order, across all
clients. A message that arrives during a reply waits for it to end.

### Outputs

| `type`     | `data` | Meaning |
|------------|--------|---------|
| `change`   | `{"agent", "patch"}` | A [JSON Patch](https://www.rfc-editor.org/rfc/rfc6902) to the agent's context. |
| `error`    | `{"agent", "message"}` | The agent's turn failed. The agent keeps running. |
| `finished` | `{"agent"}` | The agent is gone, along with its context. Nothing more comes from it. |
| `reset`    | none | The reset is done. `main` is empty and every subagent is gone. |

A subagent sends `finished` when it completes, or when `stop`, `agents.stop`
or `reset` ends it.

Every client gets every output, except an `error` about invalid input, which
goes only to the client that sent it. A new client first gets a replay of
everything sent so far, then live outputs. A client that falls 256 outputs
behind loses the ones it missed.

### Context

There are no reply messages. Bip streams changes to each agent's context as
JSON Patches using `add`, `remove` and `replace`. New messages, streamed
text and the agent's own edits all arrive this way, at any time.

A context starts as

```json
{"messages": {}, "order": []}
```

and applying an agent's `change`s in order rebuilds it. `messages` maps IDs
to messages and `order` lists the IDs in conversation order. Keying by ID
means inserting or removing a message doesn't resend the ones after it.

A message looks like this.

```json
{
  "id": "0E9258",
  "role": "assistant",
  "content": "Hey Ilia! What's up?",
  "time": "2026-11-22T01:47:53.964114Z",
  "tool_calls": [
    {"id": "call_1", "name": "lua", "arguments": "{\"code\": \"agents.spawn(...)\"}", "result": "4C1D7A"}
  ]
}
```

- `id` is 6 characters, unique in the context, and never changes.
- `role` is `"user"`, `"assistant"`, or `{"agent": "4C1D7A"}` for a subagent's
  task or report.
- `time` is when the message joined the context.
- `tool_calls` lists the `lua` calls in the message. `arguments` is the JSON
  the model wrote. `result` is `null` while the script runs, then its output.

A streamed reply looks like this.

```json
{"type": "change", "data": {"agent": "main", "patch": [
  {"op": "add", "path": "/messages/0E9258", "value": {"id": "0E9258", "role": "assistant", "content": "", "time": "...", "tool_calls": []}},
  {"op": "add", "path": "/order/2", "value": "0E9258"}
]}}
{"type": "change", "data": {"agent": "main", "patch": [{"op": "replace", "path": "/messages/0E9258/content", "value": "Hey Ilia"}]}}
{"type": "change", "data": {"agent": "main", "patch": [{"op": "replace", "path": "/messages/0E9258/content", "value": "Hey Ilia! What's up?"}]}}
```

JSON Patch can't append to a string, so each piece replaces the whole
`content`.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  http://opensource.org/licenses/MIT)

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
