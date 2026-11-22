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

| `type`    | `data` | Meaning                                    |
|-----------|--------|--------------------------------------------|
| `message` | string | A message for Bip from the person at this client. |
| `clear`   | none   | Remove every message from Bip's context. Bip publishes this as a normal `change`. |

```json
{"type": "message", "data": "Hey Bip!"}
{"type": "clear"}
```

Bip handles messages one at a time, in the order they arrive, across all
clients.

### Bip → client

| `type`   | `data` | Meaning |
|----------|--------|---------|
| `change` | array  | A [JSON Patch](https://www.rfc-editor.org/rfc/rfc6902) to Bip's context. |
| `error`  | string | Something went wrong. Bip keeps running. |

Bip doesn't send "replies". It streams changes to its context, the
conversation it sees, as JSON Patches. The system prompt (`system.md`) isn't
part of the context, so it never appears here. A new message, each streamed piece of
Bip's reply, and Bip editing its own history all arrive the same way. Bip only
uses the `add`, `remove` and `replace` operations.

The context starts as:

```json
{"messages": {}, "order": []}
```

Applying every `change` to it in order gives exactly the context Bip has.
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
  "tool_calls": []
}
```

- `id`: 6 characters, unique within the context and stable for the message's
  life.
- `role`: `"user"`, `"assistant"`, or `{"tool": "<tool call id>"}`
  for a tool's result.
- `time`: when the message joined the context.
- `tool_calls`: tools the assistant called in this message, as in the OpenAI
  Chat Completions API.

#### Example

Bip starts a reply, then streams its text:

```json
{"type": "change", "data": [
  {"op": "add", "path": "/messages/0E9258", "value": {"id": "0E9258", "role": "assistant", "content": "", "time": "...", "tool_calls": []}},
  {"op": "add", "path": "/order/2", "value": "0E9258"}
]}
{"type": "change", "data": [{"op": "replace", "path": "/messages/0E9258/content", "value": "Hey Ilia"}]}
{"type": "change", "data": [{"op": "replace", "path": "/messages/0E9258/content", "value": "Hey Ilia! What's up?"}]}
```

JSON Patch has no operation for adding to a string, so each streamed piece
replaces the message's whole `content`.
