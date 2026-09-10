merlin is a Matrix bot that reads the whole room and answers only when addressed.

It replaced a general-purpose agent framework of 961,594 lines with 2,184 lines that do the same job for one homeserver.

---

merlin is:

- **Ambient.** Every message in an allowed room is buffered. The model runs only when the bot is addressed, so unaddressed conversation costs nothing and is never written to disk.
- **Persistent.** Memory is SQLite with an FTS5 mirror. No agent or tenant foreign key, so renaming the bot is a config edit rather than a migration.
- **Single-provider.** OpenRouter for both text and images. No second vendor, no second key.
- **Sandboxed.** `run_code` runs as a user that owns nothing, with network but no LAN and no access to the bot's state or credentials.

---

## Addressing

A turn starts when a message mentions the bot and comes from an allowed sender. Everything else is context.

```
merlin what did he mean by that      -> turn (name, word boundary)
@merlin hello                        -> turn (pill or plain)
(reply to one of its messages)       -> turn
merlinesque behaviour                -> buffered only, no model call
what do you reckon                   -> buffered only, no model call
```

Matching is word-boundary, not substring. A bot that wakes on any occurrence of its name wakes on `merlin dont respond`.

Replies carry the parent message's text into the prompt. Without that, replying to a message with just the bot's name arrives blank.

## Tools

| Tool | Arguments |
| --- | --- |
| `memory_recall` | `query`, `limit` |
| `memory_store` | `key`, `content`, `category` |
| `memory_forget` | `key` |
| `web_search` | `query`, `num_results` |
| `web_fetch` | `url` |
| `http_request` | `method`, `url`, `headers`, `body` |
| `generate_image` | `prompt`, `model` |
| `run_code` | `language`, `source` |
| `cron_create` | `name`, `schedule`, `prompt`, `timezone` |
| `cron_list`, `cron_delete` | `name` |
| `time_now` | `timezone` |

There is no approval prompt. The sender allowlist is the boundary.

## Scheduled jobs

The agent creates its own jobs at runtime; nothing is declared in config.

```
cron_create(name="hn", schedule="0 7 * * *", prompt="post the top Hacker News stories")
```

Jobs live in SQLite and a reconcile loop picks them up within a minute. A firing job runs its prompt as a turn and posts the result once. One delivery path, so a digest cannot arrive twice.

## Code execution

```
sudo -u merlin-exec merlin-sandbox python   # source on stdin
```

The wrapper is the only thing merlin may reach through sudo, and it drops to a user with no files. Inside, bubblewrap unshares every namespace except the network:

- host filesystem invisible except `/nix/store` and a scratch tmpfs
- no path to the memory database, the cron store, or the environment file
- RFC1918, loopback and link-local denied by owner-matched iptables rules
- hard kill on timeout

Network is on so a script can fetch its own data. That also means executed code reaches the internet from your address.

## Configuration

Secrets never appear in the config file. It is rendered world-readable into the Nix store.

```toml
homeserver   = "https://matrix.example.org"
user_id      = "@merlin:matrix.example.org"
display_name = "merlin"

allowed_rooms   = ["!room:matrix.example.org"]
allowed_senders = ["@you:matrix.example.org"]
context_window  = 40

[model]
chat  = "z-ai/glm-5.3-flash"
image = "meta/muse-image"

[limits]
max_response_bytes = 8388608
tool_iterations    = 6
exec_timeout_s     = 60
```

From the environment: `MATRIX_PASSWORD`, `OPENROUTER_API_KEY`, `EXA_API_KEY`, optionally `MATRIX_RECOVERY_PASSPHRASE`. `MERLIN_ALLOWED_ROOMS` and `MERLIN_ALLOWED_SENDERS` override the file when the identifiers should not be published either.

The persona is a `SOUL.md` beside the config, injected on every turn.

## Running it

```sh
cargo build --release
merlin --config ./config.toml
```

Import memories from another SQLite table with `id`, `key`, `content`, `category`, `created_at`:

```sh
merlin --config ./config.toml --import-memories /path/to/old.db
```

On NixOS:

```nix
services.merlin = {
  enable = true;
  environmentFile = "/run/secrets/merlin.env";
  soul = builtins.readFile ./soul.md;
  settings = { /* as above */ };
};
```

## Notes on the dependency tree

`matrix-sdk` pins `reqwest` and `rusqlite`, and selects reqwest's `rustls` feature, which pulls `aws-lc-rs`. Cargo unifies features, so none of these can be overridden downstream. Matching its versions avoids compiling a second TLS stack and a second SQLite.

`libsqlite3-sys` declares `links = "sqlite3"`, so exactly one copy may exist in the graph.

## Tests

```sh
cargo test
```

34 tests, covering addressing, the ambient ring buffer, FTS recall and upsert, cron expression handling, the tool schemas, prompt assembly, and sandbox timeout behaviour.
