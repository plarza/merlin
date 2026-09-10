<div align="center">
<img src="docs/merlin.jpg" alt="" width="220">
<br>
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/title-dark.svg">
  <img src="docs/title-light.svg" alt="merlin" width="300">
</picture>

*a pico openclaw for matrix*
</div>



## tools

| tool | arguments |
| --- | --- |
| `memory_recall` | `query`, `limit` |
| `memory_store` | `key`, `content`, `category` |
| `memory_forget` | `key` |
| `search_messages` | `query`, `limit` |
| `send_message` | `text` |
| `read_file` | `path`, `offset`, `limit` |
| `write_file` | `path`, `content` |
| `edit_file` | `path`, `edits` |
| `list_files` | `path` |
| `grep_files` | `pattern`, `path` |
| `run_code` | `language`, `source` |
| `web_search` | `query`, `num_results` |
| `web_fetch` | `url` |
| `generate_image` | `prompt`, `model` |
| `cron_create` | `name`, `schedule`, `prompt`, `timezone` |
| `cron_list`, `cron_delete` | `name` |

there is no HTTP tool and no clock tool. the sandbox has curl, and the current time is in the system prompt, so neither earns a line in every prompt.

`send_message` posts to the room mid-turn without ending it, so a long task reports progress instead of going quiet. the final answer is still sent automatically.

## storage

three SQLite databases under the state directory.

`memory.db` holds notes the agent chose to keep: `id`, unique `key`, `content`, `category`, `room_id`, timestamps, with an FTS5 mirror. writing an existing key revises that row. there is no agent or tenant foreign key, so renaming the bot needs no migration.

`messages.db` holds every message, keyed by event id so a sync replay cannot duplicate one. two FTS5 indexes cover it, one with the default tokenizer and one with trigram.

`cron.db` holds scheduled jobs.

`memory.db` and `messages.db` each carry a sqlite-vec virtual table of embeddings keyed by rowid, recording the model and width they were built with. changing either discards the vectors and rebuilds them, since a vec0 table fixes its dimension at creation and vectors from two models cannot be compared.

## search

```
world cup "2025"             2025 required, world cup matched by meaning
"invoice"                    exact only, no embedding call
moving the hardware          pure meaning, matches "relocating the machines"
```

one syntax, borrowed from web search: a quoted term is a requirement, everything unquoted describes the subject.

quoted terms select the candidate set through the default FTS5 index. the unquoted remainder is embedded and ranks that set by cosine distance in sqlite-vec. with nothing quoted the ranking runs over the whole archive; with nothing unquoted there is no embedding call at all and BM25 order stands.

the two are never mixed into one score. BM25 ranks by term statistics and cosine ranks by direction in embedding space, so a weighted sum of them is a number that means nothing. instead each does the job it is good at: the quoted part decides what is eligible, the unquoted part decides what is best.

cosine is chosen over dot product because it ignores magnitude. Matryoshka truncation returns vectors that are not unit length, so ranking on direction alone removes a renormalisation step that would otherwise be silently wrong.

a background loop embeds whatever has no vector yet, newest first, and sleeps once it catches up. writes never wait on the network, which makes the initial backfill and steady state the same code path. until a row is embedded it is still reachable: the trigram index and Jaro-Winkler ranking from rapidfuzz remain as the fallback, so search degrades to approximate string matching rather than returning nothing.

## scheduling

```
cron_create(name="hn", schedule="0 7 * * *", prompt="post the top Hacker News stories")
```

schedules are ordinary 5-field cron expressions with an IANA timezone, validated at creation. a reconcile loop picks up additions, edits and deletions within a minute. a firing job runs its prompt as a turn and posts the result to its room once.

## sandbox

`run_code` runs bash or python inside a persistent Alpine root, reached through `sudo -u merlin-exec`, a user that owns no files.

the agent is **root inside its own root filesystem** and nothing else. `apk add`, `pip install` and `npm i` all work, and what it installs is still there next turn. that is the point: it can set up whatever it needs without anyone provisioning it.

it is root through a user namespace, so outside the namespace the kernel sees an unprivileged uid. bubblewrap unshares every namespace except the network:

- no host path is bound in at all, so `/nix/store`, `/var/lib/merlin` and `/run/secrets` are not merely unreadable, they do not exist in there
- `--cap-drop ALL`, so mounting is refused even as root
- only the safe `/dev` nodes; no block devices
- RFC1918, loopback and link-local rejected by firewall rules matched on that uid, over both IPv4 and IPv6
- `ulimit` on processes and file size, and killed at `exec_timeout_s`

the sandbox resolves DNS through public resolvers rather than the host's, because the host's nameserver is a LAN address and the LAN is exactly what it is denied.

the workspace is the one thing shared with the bot. it is a separate directory, group-owned and setgid, mounted at `/work` and the shell's starting directory, so `write_file` then `run_code` see the same tree. the sandbox's own operating system stays private to the sandbox uid.

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
chat       = "z-ai/glm-5.3-flash"
reasoning_effort = "low"
image      = "meta/muse-image"
embedding  = "google/gemini-embedding-001"
embedding_dimensions = 768

[limits]
max_response_bytes = 8388608
tool_iterations    = 32
request_timeout_s  = 120
exec_timeout_s     = 60
exec_memory_max    = "1G"
embed_batch        = 32
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

## tests

```sh
cargo test
```

integration tests against real SQLite files and real processes.
