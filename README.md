# messenger-mcp

An MCP server for a personal Facebook Messenger account, written in Rust on the
[rmcp](https://github.com/modelcontextprotocol/rust-sdk) SDK.

Status: early scaffold. Only `status` works; the chat tools are stubs.

## How it works

The client is meant to talk to Messenger the way the web client does: one WebSocket
connection to Meta's LightSpeed gateway handles the inbox and sends messages, using the
cookies of a logged-in session. Group chats work through that connection with no extra
setup. One-to-one chats are end-to-end encrypted, so the client has to register as one of
the account's devices before it can read or send there.

## Tools

| Tool | Status | Arguments | Description |
| --- | --- | --- | --- |
| `status` | working | none | Reports login and configuration state. |
| `list_chats` | stub | none | List recent conversations. |
| `read_messages` | stub | `chat_id`, `limit` | Read messages from a chat. |
| `send_message` | stub | `chat_id`, `text` | Send a text message. |

## Requirements

Rust 1.88 or later (rmcp's minimum; this crate uses edition 2024) and network access to
Meta's Messenger endpoints.

## Build and run

```sh
cargo build --release
claude mcp add -s user messenger-mcp -- /path/to/messenger-mcp/target/release/messenger-mcp
```

The binary speaks MCP over stdio. To test it by hand, pipe JSON-RPC on stdin (`initialize`,
then `tools/list`), or point any MCP client at it.

## Configuration

`MESSENGER_COOKIES`: path to a file with the cookies of a logged-in Messenger session.
Both the JSON export format and the Netscape `cookies.txt` format are accepted. `status`
reports what it finds there.

## Roadmap

1. Cookie import and the LightSpeed gateway client.
2. Read the inbox and group-chat history.
3. Send text in group chats.
4. Send images and attachments in group chats.
5. E2EE 1:1 chats: register as a device, then read and send text.
6. E2EE media.
7. A read-only mode.

## Disclaimer

Unofficial, not affiliated with Meta. Automating a personal account breaks Messenger's
terms and can get the account restricted or banned.
