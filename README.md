<div align="center">
<img src="docs/merlin.jpg" alt="" width="220">
<br>
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/title-dark.svg">
  <img src="docs/title-light.svg" alt="merlin" width="300">
</picture>
</div>

a matrix agent in rust. memory, message search, scheduled jobs, and a persistent linux sandbox.

## tools

| tool | arguments |
| --- | --- |
| `memory_recall` | `query`, `limit` |
| `memory_store` | `key`, `content`, `category` |
| `memory_forget` | `key` |
| `search_messages` | `query`, `limit` |
| `sql_query` | `query`, `limit` |
| `send_message` | `text` |
| `write_file` | `path`, `content` |
| `edit_file` | `path`, `edits` |
| `bash` | `script` |
| `web_search` | `query`, `num_results` |
| `web_fetch` | `url` |
| `generate_image` | `prompt`, `model` |
| `cron_create` | `name`, `schedule`, `prompt`, `timezone` |
| `cron_delete` | `name` |

reading, listing, searching files, HTTP requests, python and the clock are all `bash`. the current time is in the system prompt.

`send_message` posts to the room mid-turn without ending it. the final answer is sent automatically.

`edit_file` takes a list of replacements, each matched against the original file. an `old_text` that matches zero times or more than once fails the whole call, and nothing is written.

## storage

one SQLite file, `merlin.db`.

```
memories(id, key, content, category, room_id, created_at, updated_at)
messages(event_id, room_id, sender, body, at)
cron_jobs(name, schedule, timezone, prompt, room_id, enabled, created_at, last_run, last_status)
```

memory and the archive are pooled across every allowed room: `memories.room_id` records where something was learned but nothing filters on it, and `search_messages` spans all rooms. only the ambient buffer is per-room, so one room's chatter is never context in another.

`memories.key` is unique; storing an existing key updates that row. there is no agent or tenant column, so renaming the bot needs no migration. `messages.event_id` is the primary key, so a sync replay inserts nothing.

three FTS5 indexes: `memories_fts`, `messages_fts`, and `messages_trigram` with the trigram tokenizer. two sqlite-vec `vec0` tables hold embeddings keyed by rowid. `embedding_meta` records the model and width each was built with; changing either drops the table and re-embeds, since a `vec0` table fixes its dimension at creation.

`sql_query` runs `SELECT`, `WITH` and `EXPLAIN` against this file. read-only is enforced by `sqlite3_stmt_readonly`, not by inspecting the string.

memories, messages and jobs were three files before. on first start they are copied into `merlin.db` and renamed to `*.db.migrated`.

## search

```
world cup "2025"             2025 required, world cup matched by meaning
"invoice"                    exact only, no embedding call
moving the hardware          pure meaning, matches "relocating the machines"
```

quoted terms are required and select the candidate set through `messages_fts`, ranked by BM25. the unquoted remainder is embedded and reorders that set by cosine distance. with nothing quoted, the vector search runs over the whole archive. with nothing unquoted, no embedding request is made.

the two scores are not combined. BM25 ranks by term frequency, cosine by direction in embedding space; the quoted part decides eligibility, the unquoted part decides order.

cosine rather than dot product: Matryoshka truncation returns vectors that are not unit length.

a background loop embeds rows with no vector, newest first, in batches of `embed_batch`, and sleeps for 60s when there are none. writes do not wait on it. rows it has not reached fall back to trigram candidates ranked by Jaro-Winkler similarity (rapidfuzz), with a floor of 0.82.

## sandbox

`bash` runs a script in a persistent Alpine root, via `sudo -u merlin-exec`.

the process is uid 0 inside a user namespace and unprivileged outside it. `apk add`, `pip install` and `npm i` work and persist across turns.

bubblewrap unshares every namespace except the network:

- no host path is bound in; `/nix/store`, `/var/lib/merlin` and `/run/secrets` do not exist inside
- `--cap-drop ALL`; `mount` fails as root
- `--proc`, `--dev`; no block devices
- RFC1918, loopback and link-local are rejected by iptables and ip6tables rules matching the `merlin-exec` uid
- `ulimit -u 512`, `ulimit -f`, and `timeout --signal=KILL` at `exec_timeout_s`

`/etc/resolv.conf` inside the sandbox points at 1.1.1.1 and 8.8.8.8. the host's resolver is a LAN address and LAN egress is rejected.

the workspace is a separate directory, mode `2770` and group `merlin-work`, bind-mounted at `/work` and the working directory for `bash`. `write_file` and `edit_file` act on the same directory. the Alpine root itself is `0700 merlin-exec`.

## scheduling

```
cron_create(name="hn", schedule="0 7 * * *", prompt="post the top Hacker News stories")
```

5-field cron expressions with an IANA timezone, validated on creation. a reconcile loop polls `cron_jobs` every 60s and adds, replaces or removes scheduler entries. a firing job runs its prompt as a turn and posts the result to its room.

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
chat                 = "z-ai/glm-5.3-flash"
reasoning_effort     = "medium"
image                = "meta/muse-image"
embedding            = "google/gemini-embedding-001"
embedding_dimensions = 768

[limits]
max_response_bytes = 8388608
tool_iterations    = 64
request_timeout_s  = 120
exec_timeout_s     = 300
exec_memory_max    = "1G"
embed_batch        = 32
```

both allowlists fail closed: empty means none and the process exits.

`reasoning_effort` maps to OpenRouter's `reasoning.effort`. omitting it or setting `"default"` sends no field.

`request_timeout_s` is a read timeout between chunks, not a deadline on the response. completions are streamed.

credentials come from the environment. the config file is rendered world-readable into the Nix store.

| variable | required |
| --- | --- |
| `MATRIX_PASSWORD` | yes |
| `OPENROUTER_API_KEY` | yes |
| `EXA_API_KEY` | for `web_search` |
| `MATRIX_RECOVERY_PASSPHRASE` | for key backup recovery |
| `MERLIN_ALLOWED_ROOMS`, `MERLIN_ALLOWED_SENDERS` | override the config file |
| `MERLIN_EXEC_RUNNER` | override the sandbox command |

`SOUL.md` beside the config is prepended to the system prompt.

## encryption

matrix and E2EE come from mxlink over matrix-rust-sdk. the session blob is encrypted at rest with a key derived from `MATRIX_PASSWORD`.

a device holds room keys only for messages sent after it existed. `--import-keys` loads a key export from a client that has them, using `MATRIX_KEY_EXPORT_PASSPHRASE`.

## running

```sh
cargo build --release
merlin --config ./config.toml
```

| flag | effect |
| --- | --- |
| `--config <path>` | config file, default `/var/lib/merlin/config.toml` |
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

the module creates the `merlin`, `merlin-exec` and `merlin-work` accounts, a `0700` state directory, the workspace, the sudo rule, the firewall rules, and a oneshot that unpacks the Alpine root.

builds run in GitHub Actions and are pushed to a Cachix cache; the host substitutes rather than compiles. crane splits dependency compilation from the crate, so a source change does not rebuild the dependency graph.

## tests

```sh
cargo test
```

integration tests against real SQLite files and real processes.
