# messenger-mcp

An MCP server for a personal Facebook Messenger account, written in Rust on the official
[rmcp](https://github.com/modelcontextprotocol/rust-sdk) SDK.

> **Status: early scaffold.** Only `status` works; the chat tools are stubs.

## Tools

| Tool | Status | Description |
| --- | --- | --- |
| `status` | working | Reports login/config state. |
| `list_chats` | stub | List recent conversations. |
| `read_messages` | stub | Read messages from a chat. |
| `send_message` | stub | Send a text message. |

## Build and run

```sh
cargo build --release
claude mcp add -s user messenger-mcp -- /path/to/messenger-mcp/target/release/messenger-mcp
```

## Configuration

`MESSENGER_COOKIES` — path to a logged-in session's cookie file.

## Roadmap

1. Cookie import and LightSpeed gateway client.
2. Read inbox and group-chat history.
3. Send text in group chats.
4. Send images and attachments in group chats.
5. E2EE 1:1 chats: adopt the client as a device, read and send text.
6. E2EE media.

## Disclaimer

Unofficial, not affiliated with Meta. Automating a personal account violates Messenger's
terms and can get it restricted or banned.
