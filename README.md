<div align="center">
<img src="docs/merlin.jpg" alt="merlin" width="240">

# merlin
*meta ai for matrix*
</div>

A single-binary AI agent for Matrix. Tools, memory, scheduling and sandboxed code execution in ~2,200 lines of Rust and one config file.

```
merlin what did the s&p do this week   ->  searches, answers
merlin remember jakob hates mondays    ->  stores it, recalls it later
merlin backtest this on 5y of AAPL     ->  writes python, runs it sandboxed
merlin post the HN top 5 at 7am daily  ->  schedules itself
```

## Design

- **Ambient.** Every message in the room is context. The model runs only when the bot is addressed, so ordinary conversation costs nothing.
- **One provider.** OpenRouter for text and images. One key, two endpoints.
- **Flat memory.** SQLite with FTS5. No ORM, no vector database, no embeddings.
- **Sandboxed.** Generated code runs as a user that owns nothing, with internet but no LAN and no access to the bot's own state or keys.
- **Declarative.** A TOML file and an environment file. Ships with a NixOS module.

## Addressing

A turn starts when an allowed sender addresses the bot. Everything else is buffered as context.

```
merlin what did he mean by that      turn
@merlin hello                        turn
(reply to one of its messages)       turn
merlinesque behaviour                context only
what do you reckon                   context only
```

Name matching is word-boundary. Replies carry the parent message into the prompt.

## Tools

| Tool | Arguments |
| --- | --- |
| `memory_recall` | `query`, `limit` |
| `memory_store` | `key`, `content`, `category` |
| `memory_forget` | `key` |
| `search_messages` | `query`, `limit`, `fuzzy` |
| `web_search` | `query`, `num_results` |
| `web_fetch` | `url` |
| `http_request` | `method`, `url`, `headers`, `body` |
| `generate_image` | `prompt`, `model` |
| `run_code` | `language`, `source` |
| `cron_create` | `name`, `schedule`, `prompt`, `timezone` |
| `cron_list`, `cron_delete` | `name` |
| `time_now` | `timezone` |

No approval prompts. The sender allowlist is the boundary.

## Message history

Every message is archived to SQLite and searchable two ways:

```
search_messages(query="shoelace incident")               exact terms, BM25 ranked
search_messages(query="shoelase incidnt", fuzzy=true)    tolerates typos
```

Exact search is FTS5 with BM25 ranking, and falls back to fuzzy when a term finds nothing. Fuzzy gathers candidates from a trigram index, then ranks them by edit distance. No embeddings, no external service.

## Scheduled jobs

The agent schedules itself; nothing is declared in config.

```
cron_create(name="hn", schedule="0 7 * * *", prompt="post the top Hacker News stories")
```

Jobs live in SQLite and a reconcile loop picks up changes within a minute. A firing job runs its prompt as a turn and posts the result once.

## Code execution

```
sudo -u merlin-exec merlin-sandbox python   # source on stdin
```

bubblewrap unshares every namespace except the network:

- host filesystem invisible except `/nix/store` and a scratch tmpfs
- no path to the databases or the environment file
- RFC1918, loopback and link-local denied by owner-matched firewall rules
- hard kill on timeout

Network is on so scripts can fetch their own data, which also means generated code reaches the internet from your address.

## Configuration

```toml
homeserver   = "https://matrix.example.org"
user_id      = "@merlin:matrix.example.org"
display_name = "merlin"

allowed_rooms   = ["!room:matrix.example.org"]
allowed_senders = ["@you:matrix.example.org"]
context_window  = 64

[model]
chat  = "z-ai/glm-5.3-flash"
image = "meta/muse-image"

[limits]
max_response_bytes = 8388608
tool_iterations    = 16
exec_timeout_s     = 60
```

Credentials come from the environment, never the file: `MATRIX_PASSWORD`, `OPENROUTER_API_KEY`, `EXA_API_KEY`, optionally `MATRIX_RECOVERY_PASSPHRASE`. `MERLIN_ALLOWED_ROOMS` and `MERLIN_ALLOWED_SENDERS` override the file.

The persona is a `SOUL.md` beside the config, injected on every turn.

## Running it

```sh
cargo build --release
merlin --config ./config.toml
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

Import memories from another SQLite table with `id`, `key`, `content`, `category`, `created_at`:

```sh
merlin --config ./config.toml --import-memories /path/to/old.db
```

## Tests

```sh
cargo test
```
