# codex-bot

`codex-bot` is a small Rust bot that runs the local `codex` CLI behind Telegram.
It is a focused replacement for the Codex -> Telegram part of `cc-connect`.

It covers:

- Codex CLI execution and resume
- Telegram long polling
- Telegram slash commands
- Local Codex session/history management from `~/.codex`
- Compatibility with both a native bridge config and an existing `cc-connect` `[[projects]]` config

It does not cover:

- Other chat platforms
- Attachments, voice, cards, callbacks, cron, relay, or other `cc-connect` extras

## Requirements

- Rust and Cargo
- A working `codex` CLI in `PATH`
- A logged-in Codex setup under `~/.codex`
- A Telegram bot token from BotFather

## Configuration

Copy `config.example.toml` to `config.toml` and edit it:

```toml
log_level = "info"
state_path = "./codex-bot-state.json"

[telegram]
token = "123456:replace-me"
allow_from = [123456789]
group_reply_all = false
share_session_in_channel = false
poll_timeout_seconds = 30

[codex]
work_dir = "/absolute/path/to/your/project"
bin = "codex"
model = "gpt-5.4"
reasoning_effort = "high"
mode = "suggest"
extra_env = []
```

Field summary:

- `telegram.token`: your bot token
- `telegram.allow_from`: allowed Telegram user IDs; leave empty to allow anyone who can reach the bot
- `telegram.group_reply_all`: if `false`, the bot only reacts to commands, mentions, and replies in groups
- `telegram.share_session_in_channel`: if `true`, one group/channel shares one Codex session
- `codex.work_dir`: working directory passed to Codex
- `codex.bin`: Codex executable name or absolute path
- `codex.mode`: `suggest`, `full-auto`, or `yolo`

You can also point the bridge at an existing `cc-connect` config:

```bash
cargo run -- --config ~/.cc-connect/config.toml
```

If that config contains multiple Codex+Telegram projects, the parent process starts in supervisor mode and spawns one worker per project.

To run only one project from a `cc-connect` config:

```bash
cargo run -- --config ~/.cc-connect/config.toml --project "Project Name"
```

## Telegram Bot Setup

1. Open BotFather in Telegram.
2. Run `/newbot` and create a bot.
3. Copy the token into `telegram.token`.
4. Add the bot to the private chat or group where you want to use it.
5. If you keep `group_reply_all = false`, privacy mode can stay enabled.
6. If you want the bot to read all group messages with `group_reply_all = true`, disable privacy mode in BotFather with `/setprivacy`.

The bridge registers the Telegram command menu automatically at startup.

## Run

Start the bridge with a native config:

```bash
cargo run -- --config ./config.toml
```

Non-command text is forwarded to Codex as the next prompt.

## Telegram Commands

Manager frontend:

- `/help`: show help
- `/workers`: list live workers
- `/bots`: list available worker bots
- `/ls [path]`: list files from the current or specified directory
- `/cd <path>`: change the current directory for this chat binding
- `/launch <backend> <bot> <workdir>`: launch a worker in tmux

Worker frontend:

- `/help`: show help
- `/status`: show worker status
- `/stop`: stop the current request
- non-command text: submit a prompt to the attached worker session

## Notes

- RPC sockets are created under the configured `rpc.socket_dir`.
- The daemon is a small control plane: it lists live workers, lists bots, browses the filesystem, and launches workers.
- Worker inventory is derived from live processes, not a persisted daemon state store.
- Worker bots come from `telegram.worker_tokens`; the manager bot uses `telegram.manager_token`.
- Codex event parsing is intentionally tolerant of missing or drifting JSON fields to avoid the crash pattern seen in the original Go bridge.
