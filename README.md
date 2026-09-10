<div align="center">
<img src="docs/merlin.jpg" alt="merlin" width="240">

# merlin
*meta ai for matrix*
</div>

an ai agent for Matrix. one binary, one config file, about 2,800 lines of Rust.

## behaviour

every message in an allowed room is buffered and archived. a turn runs only when an allowed sender addresses the bot, so ordinary conversation costs nothing.

a message addresses the bot when it carries an `m.mentions` pill for it, contains its display name or localpart as a whole word, or replies to one of the bot's own messages. a reply passes the parent message text to the model. replies are sent as plain text in the main timeline, with no `formatted_body` and no threading.

the ambient buffer holds the last `context_window` messages per room and is refilled from the archive at startup.

## tools

| tool | arguments |
| --- | --- |
| `memory_recall` | `query`, `limit` |
| `memory_store` | `key`, `content`, `category` |
| `memory_forget` | `key` |
| `search_messages` | `query`, `limit` |
| `web_search` | `query`, `num_results` |
| `web_fetch` | `url` |
| `http_request` | `method`, `url`, `headers`, `body` |
| `generate_image` | `prompt`, `model` |
| `run_code` | `language`, `source` |
| `cron_create` | `name`, `schedule`, `prompt`, `timezone` |
| `cron_list`, `cron_delete` | `name` |
| `time_now` | `timezone` |

dispatched in a loop capped at `tool_iterations` rounds per turn. on reaching the cap the model is called once more with tools withheld, so the turn answers from what it found rather than reporting the limit. there are no approval prompts; `allowed_senders` is the only gate.

## storage

three SQLite databases under the state directory.

`memory.db` holds notes the agent chose to keep: `id`, unique `key`, `content`, `category`, `room_id`, timestamps, with an FTS5 mirror. writing an existing key revises that row. there is no agent or tenant foreign key, so renaming the bot needs no migration.

`messages.db` holds every message, keyed by event id so a sync replay cannot duplicate one. two FTS5 indexes cover it, one with the default tokenizer and one with trigram.

`cron.db` holds scheduled jobs.

## search

`search_messages` takes a query where bare words match approximately and double-quoted words are required exactly.

```
invoce                       fuzzy, matches "invoice"
"invoice"                    exact only
world cup "2025"             loose on world cup, 2025 required
```

quoted terms select the candidate set through the default FTS5 index, ranked by BM25. loose terms then rank that set by Jaro-Winkler similarity from rapidfuzz, keeping scores above 0.82. with nothing quoted, candidates come from the trigram index instead. trigram `MATCH` requires every trigram of the query to be present and so cannot match through a typo alone, which is what the ranking pass is for. an exact search that returns nothing falls back to fuzzy. no embeddings.

## scheduling

`cron_create` writes a job to SQLite at runtime; nothing is declared in config.

```
cron_create(name="hn", schedule="0 7 * * *", prompt="post the top Hacker News stories")
```

schedules are ordinary 5-field cron expressions with an IANA timezone, validated at creation. a reconcile loop picks up additions, edits and deletions within a minute. a firing job runs its prompt as a turn and posts the result to its room once.

## code execution

`run_code` accepts Python or bash. source is passed on stdin to a wrapper reached through `sudo -u merlin-exec`, a user that owns no files. the wrapper runs bubblewrap with every namespace unshared except the network:

- host filesystem invisible apart from `/nix/store` and a scratch tmpfs
- no path to the databases or the environment file
- RFC1918, loopback and link-local rejected by firewall rules matched on that uid
- killed at `exec_timeout_s`

network access is deliberate, so scripts can fetch their own data. generated code therefore reaches the internet from the host's address.

## model

OpenRouter for both text and images, over two endpoints.

chat uses `/chat/completions` with function calling and `stream: true`. streaming is required rather than cosmetic: an unstreamed request sends nothing until it completes, which makes a long generation indistinguishable from a stall and trips a total timeout. streaming allows an idle timeout instead, so a long task cannot fail merely for being long. tool calls are reassembled from deltas, and token counts come back through `stream_options`.

images use `/images`, a separate endpoint. image models do not appear in the chat model list and return 404 from `/chat/completions`. the response is base64 with a media type, uploaded to the Matrix media repository and sent as `m.image`.

## configuration

```toml
homeserver   = "https://matrix.example.org"
user_id      = "@merlin:matrix.example.org"
display_name = "merlin"

allowed_rooms   = ["!room:matrix.example.org"]
allowed_senders = ["@you:matrix.example.org"]
context_window  = 64
timezone        = "Australia/Sydney"

[model]
chat  = "z-ai/glm-5.3-flash"
image = "meta/muse-image"

[limits]
max_response_bytes = 8388608
tool_iterations    = 16
request_timeout_s  = 120
exec_timeout_s     = 60
exec_memory_max    = "1G"
```

both allowlists fail closed: empty means none, and the process refuses to start.

credentials come from the environment only, since the config file is rendered world-readable into the Nix store.

| variable | required |
| --- | --- |
| `MATRIX_PASSWORD` | yes |
| `OPENROUTER_API_KEY` | yes |
| `EXA_API_KEY` | for `web_search` |
| `MATRIX_RECOVERY_PASSPHRASE` | for key backup recovery |
| `MERLIN_ALLOWED_ROOMS`, `MERLIN_ALLOWED_SENDERS` | override the config file |
| `MERLIN_EXEC_RUNNER` | override the sandbox command |

`SOUL.md` beside the config is injected into the system prompt every turn.

## encryption

Matrix and E2EE come from mxlink over matrix-rust-sdk. the session is persisted and encrypted at rest with a key derived from the Matrix password.

a device holds room keys only for messages sent after it existed. `--import-keys` loads a key export from a client that already holds them, with the passphrase in `MATRIX_KEY_EXPORT_PASSPHRASE`, which is what makes older history readable.

## running

```sh
cargo build --release
merlin --config ./config.toml
```

| flag | effect |
| --- | --- |
| `--config <path>` | config file, default `/var/lib/merlin/config.toml` |
| `--import-memories <db>` | import from a table with `id`, `key`, `content`, `category`, `created_at`, then exit |
| `--import-keys <file>` | import an exported room key file, then exit |
| `--backfill <pages>` | page room history 100 events at a time, archive what decrypts, then exit |

on NixOS:

```nix
services.merlin = {
  enable = true;
  environmentFile = "/run/secrets/merlin.env";
  soul = builtins.readFile ./soul.md;
  settings = { /* as above */ };
};
```

the module creates the `merlin` and `merlin-exec` users, a `0700` state directory, the sudo rule for the sandbox, and the firewall rules denying it LAN access.

## dependencies

matrix-sdk pins `reqwest` and `rusqlite`, and selects reqwest's `rustls` feature, which pulls `aws-lc-rs`. cargo unifies features, so none of these can be overridden downstream. matching its versions avoids compiling a second TLS stack, and `libsqlite3-sys` declares `links = "sqlite3"`, so only one copy may exist in the graph at all. upgrading `rusqlite` past matrix-sdk's version does not compile.

## tests

```sh
cargo test
```

integration tests against real SQLite files and real processes.
