<div align="center">
<img src="docs/merlin.jpg" alt="" width="220">
<br>
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/title-dark.svg">
  <img src="docs/title-light.svg" alt="merlin" width="300">
</picture>

**a pico openclaw alternative with a pointy hat**
</div>

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

## storage

```
memories(id, key, content, category, room_id, created_at, updated_at)
messages(event_id, room_id, sender, body, at)
cron_jobs(name, schedule, timezone, prompt, room_id, enabled, created_at, last_run, last_status)
```

## search

```
world cup "2025"             2025 required, world cup matched by meaning
"invoice"                    exact only, no embedding call
moving the hardware          pure meaning, matches "relocating the machines"
```

## sandbox

`bash` runs a script in a persistent Alpine root, via `sudo -u merlin-exec`.

## scheduling

```
cron_create(name="hn", schedule="0 7 * * *", prompt="post the top Hacker News stories")
```

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
image                = "fal-ai/z-image/turbo"
image_provider       = "fal"
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

| variable | required |
| --- | --- |
| `MATRIX_PASSWORD` | yes |
| `OPENROUTER_API_KEY` | yes |
| `EXA_API_KEY` | for `web_search` |
| `FAL_API_KEY` | when `image_provider = "fal"` (`FAL_KEY` also accepted) |
| `MATRIX_RECOVERY_PASSPHRASE` | for key backup recovery |
| `MERLIN_ALLOWED_ROOMS`, `MERLIN_ALLOWED_SENDERS` | override the config file |
| `MERLIN_EXEC_RUNNER` | override the sandbox command |

`SOUL.md` is prepended to the system prompt.

## encryption

matrix and E2EE come from mxlink over matrix-rust-sdk. the session blob is encrypted at rest with a key derived from `MATRIX_PASSWORD`.

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

## tests

```sh
cargo test
```
