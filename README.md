# messenger-mcp

An MCP server for a personal Facebook Messenger account, written in Rust on the
[rmcp](https://github.com/modelcontextprotocol/rust-sdk) SDK. The Messenger client itself lives in
[messenger-cli](https://github.com/Beirun/messenger-cli), which also installs a `messenger` command
over the same cookies and browser profile.

Status: early development. Browser reading returns the messages currently loaded in Messenger.

## How it works

The client talks to Messenger the way the web client does: one WebSocket connection to Meta's
LightSpeed gateway handles the inbox and sends messages, using the cookies of a logged-in
session. Group chats work through that connection with no extra setup. One-to-one chats are
end-to-end encrypted, and Meta drops plaintext messages sent to them. Those go through the
`browser` backend instead, which drives the Chrome profile that `login` created and lets the
web client do the encryption.

## Tools

| Tool | Status | Arguments | Description |
| --- | --- | --- | --- |
| `status` | working | none | Checks the saved cookies against Messenger and reports the session state. |
| `list_chats` | working | none | List recent conversations, naming direct chats after the other participant. |
| `read_messages` | experimental | `chat_id`, `limit`, `backend` | Use `browser` for encrypted conversations or `gateway` (default) for supported chats. |
| `send_message` | working | `chat_id`, `text`, `backend` | Send a text message and read the thread back to confirm it arrived. Use `browser` for encrypted conversations. |

## Setup

### 1. Prerequisites

- Rust 1.100 or newer, installed through rustup. `rust-toolchain.toml` pins the exact
  version, so `cargo` uses it without further configuration.
- A Chrome or Chromium binary. It is only needed for `login` and for the browser backend;
  the gateway tools never launch one. The binary is looked up in `MESSENGER_CHROME`, then in
  an agent-browser install, then on `PATH`.
- Network access to Meta's Messenger endpoints, and to GitHub when Cargo fetches the
  `messenger-cli` dependency.

The built binary links against glibc, `libm` and `libgcc` only. There is no OpenSSL,
database, Node or Python dependency.

### 2. Build

```sh
cargo build --release
```

The result is `target/release/messenger-mcp`. It speaks MCP over stdio and accepts one
optional subcommand, `login`.

### 3. Sign in and save the session

```sh
./target/release/messenger-mcp login
```

Chrome opens at messenger.com and waits for you to sign in. When the session exists it writes
`~/.config/messenger-mcp/cookies.json` with `600` permissions and closes the window. It waits
300 seconds by default; raise `MESSENGER_LOGIN_TIMEOUT` if you need longer.

- If Messenger asks for your PIN to restore encrypted chats, enter it in that window. The
  profile at `~/.local/share/messenger-mcp/chrome-profile` keeps the encrypted state, and the
  browser backend reuses it later without asking again.
- Chrome for Testing rejects Google sign-in with "This browser or app may not be secure". Use
  the email and password form, or point `MESSENGER_CHROME` at a regular Chrome install.
- Only one process may use that profile at a time. A stale `SingletonLock` in it makes the
  browser tools refuse to start until Chrome using it is closed.

### 4. Register the server

```sh
claude mcp add -s user messenger-mcp -- "$PWD/target/release/messenger-mcp"
```

Other MCP clients need a stdio entry with no arguments:

```json
{
  "mcpServers": {
    "messenger-mcp": {
      "command": "/path/to/messenger-mcp/target/release/messenger-mcp"
    }
  }
}
```

### 5. Check it works

Start with `status`. It reports the cookie path, whether `c_user` and `xs` are present, and
what Messenger says about the session. `list_chats` should then return recent conversations.
For an encrypted conversation, call `read_messages` with `"backend": "browser"`; sending in
one takes `send_message` with the same backend.

## Limits of the browser backend

It returns only the messages currently rendered in the conversation, so long histories are
partial. Opening a chat can mark it read. Sends reload the thread and report success only if
the message is rendered afterwards. Every browser call holds the profile exclusively, so two
calls cannot run at once.

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
5. E2EE 1:1 chats without the browser: register as a device for native reads and sends.
6. E2EE media.
7. A read-only mode.

## Disclaimer

Unofficial, not affiliated with Meta. Automating a personal account breaks Messenger's
terms and can get the account restricted or banned.

## License

AGPL-3.0-only. The LightSpeed protocol client is
[`messenger-client`](https://github.com/rgryta/messenger-sync), which is AGPL-3.0 because it
was ported from mautrix-meta, so this project carries the same license.
