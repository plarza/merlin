<div align="center">
<img src="docs/merlin.jpg" alt="merlin" width="240">

# merlin
*meta ai for matrix*
</div>

a single-binary ai agent for Matrix. about 2,200 lines of Rust, one config file, no plugin system.

```
merlin what did the s&p do this week   searches, answers
merlin remember jakob hates mondays    stores it, recalls it later
merlin backtest this on 5y of AAPL     writes Python, runs it sandboxed
merlin post the HN top 5 at 7am daily  schedules itself
```

## how it works

merlin sits in a room and reads everything. the model only runs when someone addresses it, so ordinary conversation between people costs nothing.

when it does run it has tools: its own memory, the message history, web search, arbitrary http, image generation, a code sandbox, and a scheduler it can write to.

```
merlin what did he mean by that      runs a turn
@merlin hello                        runs a turn
(reply to one of its messages)       runs a turn
merlinesque behaviour                context only
what do you reckon                   context only
```

name matching is word-boundary, so the bird and the wizard do not wake it. replying to a message hands the parent text to the model along with the reply.

## tools

| tool | arguments |
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

there are no approval prompts. the sender allowlist is the only gate.

## memory and history

two separate stores. memory holds notes the agent decided to keep. history holds every message anyone sent.

```
search_messages(query="shoelace incident")
search_messages(query="shoelase incidnt", fuzzy=true)
```

exact search is FTS5 ranked by BM25, and falls back to fuzzy when a term finds nothing. fuzzy pulls candidates from a trigram index and ranks them by Levenshtein distance. trigram MATCH on its own wants every trigram of the query present, which a typo breaks, so the ranking pass is what makes misspellings work. no embeddings involved.

## scheduling

the agent writes its own jobs at runtime.

```
cron_create(name="hn", schedule="0 7 * * *", prompt="post the top Hacker News stories")
```

jobs live in SQLite. a reconcile loop notices additions and edits within a minute, so a new job does not wait for a restart. when one fires it runs the prompt as a turn and posts the result once.

## code execution

```
sudo -u merlin-exec merlin-sandbox python   # source arrives on stdin
```

generated code runs as a separate user that owns no files, under bubblewrap with every namespace unshared except the network:

- the host filesystem is invisible apart from `/nix/store` and a scratch tmpfs
- there is no path to the databases or the environment file holding the keys
- RFC1918, loopback and link-local are rejected by firewall rules matched on that uid
- the process is killed at the timeout

the network stays on so a script can fetch its own data. that also means generated code can reach the internet from your address.

## configuration

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

credentials come from the environment and never the file, since the file gets rendered world-readable into the Nix store: `MATRIX_PASSWORD`, `OPENROUTER_API_KEY`, `EXA_API_KEY`, and optionally `MATRIX_RECOVERY_PASSPHRASE`. `MERLIN_ALLOWED_ROOMS` and `MERLIN_ALLOWED_SENDERS` override the file when the identifiers should stay out of a public repo too.

the persona is a `SOUL.md` next to the config. it gets injected on every turn.

## running it

```sh
cargo build --release
merlin --config ./config.toml
```

on NixOS:

```nix
services.merlin = {
  enable = true;
  environmentFile = "/run/secrets/merlin.env";
  soul = builtins.readFile ./soul.md;
  settings = { /* as above */ };
};
```

importing memories from another SQLite table with `id`, `key`, `content`, `category`, `created_at`:

```sh
merlin --config ./config.toml --import-memories /path/to/old.db
```

## tests

```sh
cargo test
```
