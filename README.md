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

## slash commands

Slash commands must be the entire message. They are handled immediately without
calling the chat model.

| Command | Effect |
| --- | --- |
| `/help` | List available slash commands |
| `/model` | Show the current chat model |
| `/model <model-slug>` | Switch the chat model for subsequent turns until Merlin restarts |
| `/reasoning` | Show the current reasoning effort |
| `/reasoning <effort>` | Switch the reasoning effort for subsequent turns until Merlin restarts |

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

## setup

**1. matrix.** register an account for the bot on your homeserver and sign into it once from a normal client. add each room's internal id (`!abc:matrix.example.org`, not the `#alias`) to `allowed_rooms`, then invite the bot; merlin accepts allowlisted invitations and rejects the rest. keep the normal client session: merlin keeps no key backup, so that session is where room keys for older history have to come from.

**2. keys.** `OPENROUTER_API_KEY` drives both chat and embeddings, so it is never optional. `EXA_API_KEY` is only read by `web_search`, `FAL_API_KEY` only when `image_provider = "fal"`.

**3. config.** write the TOML below to `config.toml`, with `allowed_rooms` and `allowed_senders` filled in — both are rejected empty, since the bot would either join nothing or answer nobody. secrets stay in the environment; nothing in this file is private.

**4a. nixos.** the module is the whole deployment: it creates the `merlin` and `merlin-exec` users, unpacks the sandbox root, writes the sudo rule that joins them, and firewalls executed code off the LAN.

```nix
{
  inputs.merlin.url = "github:plarza/merlin";

  outputs = { nixpkgs, merlin, ... }: {
    nixosConfigurations.host = nixpkgs.lib.nixosSystem {
      modules = [ merlin.nixosModules.default ./merlin.nix ];
    };
  };
}
```

`./merlin.nix` carries the `services.merlin` block from [running](#running). point `environmentFile` at a file of `KEY=value` lines readable only by root.

**4b. anywhere else.** `cargo build --release`, then create the state directory and hand it to the user merlin runs as:

```sh
install -d -m 0700 -o merlin -g merlin /var/lib/merlin
install -d -m 2770 -o merlin -g merlin /var/lib/merlin-workspace
```

the default sandbox runner is a NixOS path, so set `MERLIN_EXEC_RUNNER` to your own bubblewrap or container wrapper — it is invoked with a timeout in seconds and an address-space limit in kilobytes, and takes the script on stdin. leaving it unset costs you `bash` alone; every other tool still works.

**5. first run.** start it and wait for `connected` in the log. history from before the bot joined is invisible until you page it in:

```sh
merlin --config ./config.toml --backfill 50
```

if that reports mostly undecryptable events, this device simply has no room keys for them: export the keys from the client you set up in step 1 and `--import-keys` them, with the export passphrase in `MATRIX_KEY_EXPORT_PASSPHRASE`.

> the session blob is keyed on `MATRIX_PASSWORD`, so rotating the password orphans it and forces a new device. set `SESSION_ENCRYPTION_KEY` to something stable up front if you expect to rotate.

## configuration

```toml
homeserver   = "https://matrix.example.org"
user_id      = "@merlin:matrix.example.org"
display_name = "merlin"

allowed_rooms   = ["!room:matrix.example.org"]
allowed_senders = ["@you:matrix.example.org"]
context_window  = 64
timezone        = "Australia/Sydney"
state_dir       = "/var/lib/merlin"

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
| `MATRIX_KEY_EXPORT_PASSPHRASE` | for `--import-keys` |
| `SESSION_ENCRYPTION_KEY` | no, defaults to `MATRIX_PASSWORD` |
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
