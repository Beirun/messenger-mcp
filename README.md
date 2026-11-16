# messenger-mcp

An MCP server for a personal Facebook Messenger account, written in Rust on the
[rmcp](https://github.com/modelcontextprotocol/rust-sdk) SDK.

Status: early development. Browser reading returns the messages currently loaded in Messenger.

## How it works

The client is meant to talk to Messenger the way the web client does: one WebSocket
connection to Meta's LightSpeed gateway handles the inbox and sends messages, using the
cookies of a logged-in session. Group chats work through that connection with no extra
setup. One-to-one chats are end-to-end encrypted, so the client has to register as one of
the account's devices before it can read or send there.

## Tools

| Tool | Status | Arguments | Description |
| --- | --- | --- | --- |
| `status` | working | none | Checks the saved cookies against Messenger and reports the session state. |
| `list_chats` | working | none | List recent conversations, naming direct chats after the other participant. |
| `read_messages` | experimental | `chat_id`, `limit`, `backend` | Use `browser` for encrypted conversations or `gateway` (default) for supported chats. |
| `send_message` | working for group chats | `chat_id`, `text` | Send a text message and read the thread back to confirm it arrived. |

## Requirements

Rust 1.88 or later (rmcp's minimum; this crate uses edition 2024) and network access to
Meta's Messenger endpoints.

Browser reads also need a Chrome or Chromium binary and the profile that `login` creates.
Encrypted conversations require unlocking that profile once with your Messenger PIN. The
browser backend returns only the messages currently rendered in the conversation, so long
histories are partial, and opening a chat can mark it read.

## Build and run

```sh
cargo build --release
```

Sign in once and save the session cookies:

```sh
messenger-mcp login
```

That opens Chrome at messenger.com and waits while you sign in. When the session appears,
it saves the cookies with `600` permissions and closes the window. It uses the Chrome for
Testing that agent-browser installed, falling back to a system Chrome.

Then register the server:

```sh
claude mcp add -s user messenger-mcp -- /path/to/messenger-mcp/target/release/messenger-mcp
```

The binary speaks MCP over stdio. To test it by hand, pipe JSON-RPC on stdin (`initialize`,
then `tools/list`), or point any MCP client at it.

## Configuration

| Variable | Default | Purpose |
| --- | --- | --- |
| `MESSENGER_COOKIES` | `~/.config/messenger-mcp/cookies.json` | Where `login` saves cookies and `status` reads them. JSON export and Netscape `cookies.txt` are both accepted. |
| `MESSENGER_CHROME` | agent-browser's Chrome, then system Chrome | Chrome binary used by `login`. |
| `MESSENGER_LOGIN_TIMEOUT` | `300` | Seconds `login` waits for sign-in. |

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

## License

AGPL-3.0-only. The LightSpeed protocol client is
[`messenger-client`](https://github.com/rgryta/messenger-sync), which is AGPL-3.0 because it
was ported from mautrix-meta, so this project carries the same license.
