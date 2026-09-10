//! End to end tests against real databases and real processes.
//!
//! Everything here goes through the public API on real files, so a passing run means the SQLite schema, the FTS indexes and the process plumbing actually work,
//! not that a helper returns what it was told to.

use merlin::cron::{CronStore, Job};
use merlin::embed::Embeddable;
use merlin::exec::Sandbox;
use merlin::llm::{Attachment, Message};
use merlin::memory::Memory;
use merlin::messages::Archive;
use merlin::room::{Buffers, Turn, is_addressed};
use merlin::tools::definitions;

/// A unique directory per test, so runs do not share state.
fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "merlin-test-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ── memory ──────────────────────────────────────────────────────────────────

#[test]
fn memory_survives_reopening_and_recalls_by_keyword() {
    let dir = scratch("memory");
    let path = dir.join("memory.db");

    {
        let m = Memory::open(&path).unwrap();
        m.store(
            "kettle",
            "the kettle in the kitchen is broken",
            "core",
            None,
        )
        .unwrap();
        m.store("parking", "visitor parking is free after six", "core", None)
            .unwrap();
        // Re-filing the same subject revises it.
        m.store(
            "kettle",
            "the kettle is broken. a new one is on order",
            "core",
            None,
        )
        .unwrap();
    }

    let m = Memory::open(&path).unwrap();
    assert_eq!(m.count().unwrap(), 2, "upsert must not duplicate a key");

    let hits = m.recall("kettle broken", None, 5).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(
        hits[0].content.contains("new one is on order"),
        "kept the revision"
    );

    // A query that FTS5 would reject as syntax must still be answerable.
    assert!(m.recall("!!!", None, 5).is_ok());

    assert!(m.forget("kettle").unwrap());
    assert!(!m.forget("kettle").unwrap());
    assert_eq!(m.count().unwrap(), 1);
}

// ── message search ──────────────────────────────────────────────────────────

fn seeded_archive(dir: &std::path::Path) -> Archive {
    let a = Archive::open(&dir.join("messages.db")).unwrap();
    for (i, (sender, body)) in [
        ("@alice:example.org", "the shoelace came apart again"),
        ("@bob:example.org", "the gymnast landed it cleanly"),
        ("@alice:example.org", "who won the 2025 world cup"),
        ("@bob:example.org", "the 2018 world cup final was dull"),
    ]
    .iter()
    .enumerate()
    {
        a.record(
            &format!("$e{i}"),
            "!r:example.org",
            sender,
            body,
            "2026-09-10T00:00:00Z",
        )
        .unwrap();
    }
    a
}

#[test]
fn bare_words_tolerate_misspelling() {
    let dir = scratch("fuzzy");
    let hits = seeded_archive(&dir).search("shoelase", None, 5).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].body.contains("shoelace"));
}

#[test]
fn quoted_terms_are_required_and_combine_with_fuzzy_ones() {
    let dir = scratch("quoted");
    let a = seeded_archive(&dir);

    // Both messages concern the world cup, and the quoted year selects one.
    let hits = a.search("wrold cup \"2025\"", None, 5).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].body.contains("2025"));

    // A misspelled loose term alongside a required one.
    let hits = a.search("finl \"2018\"", None, 5).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].body.contains("2018"));

    // Quoting alone is an exact search, so a near miss finds nothing.
    assert_eq!(a.search("\"gymnast\"", None, 5).unwrap().len(), 1);
    assert!(a.search("\"gymnasts\"", None, 5).unwrap().is_empty());
}

#[test]
fn search_rejects_nothing_and_finds_nothing_for_unrelated_queries() {
    let dir = scratch("edges");
    let a = seeded_archive(&dir);
    assert!(a.search("s&p", None, 5).is_ok());
    assert!(a.search("???", None, 5).is_ok());
    assert!(a.search("", None, 5).unwrap().is_empty());
    assert!(a.search("elephant", None, 5).unwrap().is_empty());
}

#[test]
fn recent_returns_the_newest_messages_oldest_first() {
    let dir = scratch("recent");
    let a = seeded_archive(&dir);
    let rows = a.recent("!r:example.org", 2).unwrap();
    assert_eq!(rows.len(), 2);
    // Ordering matters: the buffer is rebuilt from this and reads as conversation.
    assert!(rows[0].at <= rows[1].at);
    // A room with nothing in it restores nothing rather than failing.
    assert!(a.recent("!empty:example.org", 5).unwrap().is_empty());
}

#[test]
fn an_event_is_archived_once_however_often_sync_replays_it() {
    let dir = scratch("dupes");
    let a = seeded_archive(&dir);
    let before = a.count().unwrap();
    a.record(
        "$e0",
        "!r:example.org",
        "@alice:example.org",
        "different text",
        "now",
    )
    .unwrap();
    assert_eq!(a.count().unwrap(), before);
}

// ── semantic search ─────────────────────────────────────────────────────────

/// A stand-in embedding with three axes the tests can reason about, so a "nearest" result is checkable rather than opaque.
/// The real model returns 768 components; the storage and ranking path is identical either way.
fn vector(x: f32, y: f32, z: f32) -> Vec<f32> {
    vec![x, y, z]
}

const MODEL: &str = "test/embedding-model";

#[test]
fn semantic_recall_ranks_memories_by_direction_not_magnitude() {
    let dir = scratch("semantic-memory");
    let mut m = Memory::open(&dir.join("memory.db")).unwrap();
    m.enable_semantic(MODEL, 3).unwrap();

    m.store("boiler", "the heating packed up", "core", None)
        .unwrap();
    m.store("parking", "visitor parking is free after six", "core", None)
        .unwrap();

    let pending = m.pending_embeddings(10).unwrap();
    assert_eq!(pending.len(), 2, "nothing is embedded until the loop runs");

    let vectors: Vec<(i64, Vec<f32>)> = pending
        .iter()
        .map(|(id, text)| {
            // Deliberately not unit length: Matryoshka truncation returns short vectors, so cosine has to rank on direction alone.
            let v = if text.contains("heating") {
                vector(0.31, 0.0, 0.0)
            } else {
                vector(0.0, 1.0, 0.0)
            };
            (*id, v)
        })
        .collect();
    assert_eq!(m.save_embeddings(&vectors).unwrap(), 2);
    assert!(m.pending_embeddings(10).unwrap().is_empty());

    // A query along the heating axis at a wildly different scale.
    let hits = m
        .recall("heating trouble", Some(&vector(9.7, 0.0, 0.0)), 2)
        .unwrap();
    assert_eq!(hits[0].key, "boiler");
    assert_eq!(hits[1].key, "parking");
}

#[test]
fn revising_a_memory_re_embeds_it() {
    let dir = scratch("semantic-revise");
    let mut m = Memory::open(&dir.join("memory.db")).unwrap();
    m.enable_semantic(MODEL, 3).unwrap();

    m.store("kettle", "the kettle is broken", "core", None)
        .unwrap();
    let pending = m.pending_embeddings(10).unwrap();
    m.save_embeddings(&[(pending[0].0, vector(1.0, 0.0, 0.0))])
        .unwrap();
    assert!(m.pending_embeddings(10).unwrap().is_empty());

    // The row keeps its rowid, so a stale vector would go on describing the old text.
    m.store("kettle", "the kettle was replaced on tuesday", "core", None)
        .unwrap();
    assert_eq!(
        m.pending_embeddings(10).unwrap().len(),
        1,
        "revised content must be queued for re-embedding"
    );
}

#[test]
fn forgetting_a_memory_takes_its_vector_with_it() {
    let dir = scratch("semantic-forget");
    let mut m = Memory::open(&dir.join("memory.db")).unwrap();
    m.enable_semantic(MODEL, 3).unwrap();

    m.store("doomed", "this will be deleted", "core", None)
        .unwrap();
    let pending = m.pending_embeddings(10).unwrap();
    m.save_embeddings(&[(pending[0].0, vector(1.0, 0.0, 0.0))])
        .unwrap();

    m.forget("doomed").unwrap();

    let hits = m
        .recall("deleted thing", Some(&vector(1.0, 0.0, 0.0)), 5)
        .unwrap();
    assert!(
        hits.iter().all(|h| h.key != "doomed"),
        "a deleted memory must not still be reachable by meaning, found {hits:?}"
    );

    // SQLite hands the freed rowid to the next insert, so a surviving vector would answer for an unrelated memory.
    m.store("fresh", "something else entirely", "core", None)
        .unwrap();
    assert_eq!(
        m.pending_embeddings(10).unwrap().len(),
        1,
        "the reused rowid must not inherit a vector"
    );
}

#[test]
fn changing_the_embedding_model_discards_incompatible_vectors() {
    let dir = scratch("semantic-model-swap");
    let path = dir.join("memory.db");

    {
        let mut m = Memory::open(&path).unwrap();
        m.enable_semantic(MODEL, 3).unwrap();
        m.store("a", "some note", "core", None).unwrap();
        let pending = m.pending_embeddings(10).unwrap();
        m.save_embeddings(&[(pending[0].0, vector(1.0, 0.0, 0.0))])
            .unwrap();
        assert!(m.pending_embeddings(10).unwrap().is_empty());
    }

    // A different width cannot be compared against the stored vectors at all.
    let m = Memory::open(&path).unwrap();
    m.enable_semantic("test/other-model", 4).unwrap();
    assert_eq!(
        m.pending_embeddings(10).unwrap().len(),
        1,
        "vectors from another model must be rebuilt, not reused"
    );
}

#[test]
fn quoted_terms_filter_and_the_rest_ranks_by_meaning() {
    let dir = scratch("semantic-combined");
    let mut a = Archive::open(&dir.join("messages.db")).unwrap();
    a.enable_semantic(MODEL, 3).unwrap();

    // Two mention 2025, one does not. Two are about the world cup, one is not.
    let rows = [
        ("$a", "@sam:x", "the world cup final was in 2025"),
        ("$b", "@lee:x", "quarterly revenue for 2025 was strong"),
        ("$c", "@kim:x", "the world cup was thrilling"),
    ];
    for (i, (id, sender, body)) in rows.iter().enumerate() {
        a.record(
            id,
            "!r:x",
            sender,
            body,
            &format!("2026-01-0{}T00:00:00Z", i + 1),
        )
        .unwrap();
    }

    let pending = a.pending_embeddings(10).unwrap();
    let vectors: Vec<(i64, Vec<f32>)> = pending
        .iter()
        .map(|(id, text)| {
            let v = if text.contains("world cup") {
                vector(1.0, 0.0, 0.0)
            } else {
                vector(0.0, 1.0, 0.0)
            };
            (*id, v)
        })
        .collect();
    a.save_embeddings(&vectors).unwrap();

    // `world cup "2025"`: 2025 is a requirement, the rest is meaning.
    let hits = a
        .search("world cup \"2025\"", Some(&vector(1.0, 0.0, 0.0)), 5)
        .unwrap();

    let bodies: Vec<&str> = hits.iter().map(|h| h.body.as_str()).collect();
    assert!(
        !bodies.contains(&"the world cup was thrilling"),
        "a message without the required term must be excluded however well it matches in meaning, got {bodies:?}"
    );
    assert_eq!(
        bodies,
        vec![
            "the world cup final was in 2025",
            "quarterly revenue for 2025 was strong"
        ],
        "both keep 2025; the world cup one ranks first on meaning"
    );
}

#[test]
fn an_all_quoted_query_stays_exact() {
    let dir = scratch("semantic-quoted-only");
    let mut a = Archive::open(&dir.join("messages.db")).unwrap();
    a.enable_semantic(MODEL, 3).unwrap();
    a.record(
        "$a",
        "!r:x",
        "@sam:x",
        "the invoice went out friday",
        "2026-01-01T00:00:00Z",
    )
    .unwrap();
    a.record(
        "$b",
        "!r:x",
        "@lee:x",
        "lunch was excellent",
        "2026-01-02T00:00:00Z",
    )
    .unwrap();
    let pending = a.pending_embeddings(10).unwrap();
    let vectors: Vec<(i64, Vec<f32>)> = pending
        .iter()
        .map(|(id, _)| (*id, vector(1.0, 0.0, 0.0)))
        .collect();
    a.save_embeddings(&vectors).unwrap();

    // Nothing unquoted means nothing to embed, so identical vectors cannot muddle the result.
    let hits = a.search("\"invoice\"", None, 5).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].sender, "@sam:x");
}

#[test]
fn search_falls_back_to_matching_text_when_nothing_is_embedded_yet() {
    let dir = scratch("semantic-backfilling");
    let a = Archive::open(&dir.join("messages.db")).unwrap();
    a.enable_semantic(MODEL, 3).unwrap();
    a.record(
        "$a",
        "!r:x",
        "@sam:x",
        "the shoelace snapped",
        "2026-01-01T00:00:00Z",
    )
    .unwrap();

    // Mid-backfill every row is pending, so ranking by meaning has nothing to work with.
    // Returning nothing until it finishes would be worse than approximate matches.
    let hits = a
        .search("shoelase", Some(&vector(1.0, 0.0, 0.0)), 5)
        .unwrap();
    assert_eq!(hits.len(), 1, "the fallback must still answer");
    assert_eq!(hits[0].body, "the shoelace snapped");
}

// ── addressing ──────────────────────────────────────────────────────────────

const UID: &str = "@merlin:example.org";

fn addressed(body: &str) -> bool {
    is_addressed(body, &[], UID, "merlin", "merlin", false)
}

#[test]
fn a_turn_starts_only_when_the_bot_is_actually_addressed() {
    assert!(addressed("merlin what day is it"));
    assert!(addressed("hey Merlin, you there?"));
    assert!(addressed("@merlin hello"));
    assert!(addressed("ask @merlin:example.org about it"));

    // Word boundary, so the bird and the wizard do not wake it.
    assert!(!addressed("merlinesque behaviour"));
    assert!(!addressed("submerlin"));
    assert!(!addressed("what do you reckon about the game"));
    assert!(!addressed(""));

    // A reply to the bot counts even with no name in the body.
    assert!(is_addressed(
        "what did you mean",
        &[],
        UID,
        "merlin",
        "merlin",
        true
    ));
}

#[test]
fn an_explicit_mention_list_decides_in_both_directions() {
    assert!(is_addressed(
        "can you look at this",
        &[UID.into()],
        UID,
        "merlin",
        "merlin",
        false
    ));
    assert!(!is_addressed(
        "merlin is a bird",
        &["@bob:example.org".to_string()],
        UID,
        "merlin",
        "merlin",
        false
    ));
}

#[test]
fn ambient_context_keeps_the_newest_messages_per_room() {
    let b = Buffers::new(3);
    for i in 0..5 {
        b.push(
            "!a",
            Turn {
                sender: "@x".into(),
                body: i.to_string(),
            },
        );
    }
    b.push(
        "!b",
        Turn {
            sender: "@y".into(),
            body: "other room".into(),
        },
    );

    let rendered = b.render("!a", false).unwrap();
    assert_eq!(
        rendered, "@x: 2\n@x: 3\n@x: 4",
        "oldest evicted, sender labelled"
    );

    // skip_last drops the message being answered.
    assert_eq!(b.render("!a", true).unwrap(), "@x: 2\n@x: 3");
    assert_eq!(b.render("!b", false).unwrap(), "@y: other room");
    assert!(b.render("!missing", false).is_none());
}

// ── scheduling ──────────────────────────────────────────────────────────────

fn job(name: &str, schedule: &str) -> Job {
    Job {
        name: name.into(),
        schedule: schedule.into(),
        timezone: "Australia/Sydney".into(),
        prompt: "post the digest".into(),
        room_id: "!r:example.org".into(),
        enabled: true,
    }
}

#[test]
fn jobs_persist_edit_in_place_and_delete() {
    let dir = scratch("cron");
    let path = dir.join("cron.db");

    {
        let s = CronStore::open(&path).unwrap();
        s.upsert(&job("hn", "0 7 * * *")).unwrap();
        s.upsert(&job("hn", "0 8 * * *")).unwrap();
        s.record_run("hn", "ok").unwrap();
    }

    let s = CronStore::open(&path).unwrap();
    let jobs = s.list().unwrap();
    assert_eq!(jobs.len(), 1, "same name edits rather than duplicates");
    assert_eq!(jobs[0].schedule, "0 8 * * *");

    assert!(s.delete("hn").unwrap());
    assert!(!s.delete("hn").unwrap());
}

#[test]
fn crontab_syntax_is_accepted_and_nonsense_is_refused() {
    assert!(job("hn", "0 7 * * *").validate().is_ok());
    // Five fields gain a seconds column for the scheduler.
    assert_eq!(job("hn", "0 7 * * *").six_field_schedule(), "0 0 7 * * *");
    assert_eq!(
        job("hn", "30 0 7 * * *").six_field_schedule(),
        "30 0 7 * * *"
    );

    assert!(job("hn", "not a cron").validate().is_err());
    let mut bad_zone = job("hn", "0 7 * * *");
    bad_zone.timezone = "Mars/Olympus".into();
    assert!(bad_zone.validate().is_err());
    let mut unnamed = job("  ", "0 7 * * *");
    unnamed.name = "  ".into();
    assert!(unnamed.validate().is_err());
}

// ── sandbox ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_sandbox_runs_a_program_and_returns_its_output() {
    let sb = Sandbox::new(
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "cat >/dev/null; echo 42".into(),
        ],
        10,
        "1G".into(),
    );
    let out = sb.run("python", "print(6*7)", None).await.unwrap();
    assert_eq!(out.stdout.trim(), "42");
    assert!(!out.timed_out);
}

#[tokio::test]
async fn a_wedged_program_is_killed_rather_than_hanging_the_turn() {
    let sb = Sandbox::new(
        vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
        1,
        "1G".into(),
    );
    let out = sb.run("bash", "true", None).await.unwrap();
    assert!(out.timed_out);
    assert!(out.stderr.contains("killed"));
}

#[tokio::test]
async fn an_unsupported_language_is_refused_before_spawning() {
    let sb = Sandbox::new(vec!["/bin/false".into()], 5, "1G".into());
    assert!(sb.run("ruby", "puts 1", None).await.is_err());
}

// ── model wire format ───────────────────────────────────────────────────────

#[test]
fn tool_results_and_calls_match_the_openai_wire_format() {
    let result = serde_json::to_value(Message::tool_result("call_1", "42")).unwrap();
    assert_eq!(result["role"], "tool");
    assert_eq!(result["tool_call_id"], "call_1");
    // An empty tool_calls list must not appear on a tool result.
    assert!(result.get("tool_calls").is_none());

    let parsed: Message = serde_json::from_value(serde_json::json!({
        "role": "assistant",
        "content": null,
        "tool_calls": [{
            "id": "c1",
            "type": "function",
            "function": { "name": "memory_recall", "arguments": "{\"query\":\"kettle\"}" }
        }]
    }))
    .unwrap();
    assert_eq!(parsed.tool_calls[0].function.name, "memory_recall");
}

#[test]
fn a_message_with_images_serialises_as_multipart_content() {
    let m = Message::user_with_images(
        "look at this",
        &[Attachment {
            bytes: vec![1, 2, 3],
            media_type: "image/png".into(),
        }],
    );
    let v = serde_json::to_value(&m).unwrap();
    let parts = v["content"].as_array().expect("content must be an array");
    assert_eq!(parts[0]["type"], "text");
    assert_eq!(parts[0]["text"], "look at this");
    assert_eq!(parts[1]["type"], "image_url");
    // Inlined as a data URI carrying the declared media type.
    let url = parts[1]["image_url"]["url"].as_str().unwrap();
    assert!(url.starts_with("data:image/png;base64,"));

    // A message without images stays a plain string, which is what the API expects.
    let plain = serde_json::to_value(Message::user("hello")).unwrap();
    assert!(plain["content"].is_string());
}

#[test]
fn every_tool_the_agent_is_offered_is_described() {
    let names: Vec<String> = definitions()
        .iter()
        .map(|d| {
            let f = &d["function"];
            assert_eq!(f["parameters"]["type"], "object");
            // The description is the only thing telling the model when to reach for a tool, so an empty one is a real defect.
            assert!(f["description"].as_str().unwrap().len() > 20);
            f["name"].as_str().unwrap().to_string()
        })
        .collect();

    for expected in [
        "memory_store",
        "memory_recall",
        "memory_forget",
        "search_messages",
        "web_search",
        "web_fetch",
        "generate_image",
        "send_message",
        "write_file",
        "edit_file",
        "run_code",
        "cron_create",
        "cron_list",
        "cron_delete",
    ] {
        assert!(names.contains(&expected.to_string()), "missing {expected}");
    }

    // Dropped once the sandbox gained a shell: curl and date do these jobs, and
    // every extra tool is another line in every prompt.
    for gone in [
        "http_request",
        "time_now",
        "read_file",
        "list_files",
        "grep_files",
    ] {
        assert!(!names.contains(&gone.to_string()), "{gone} should be gone");
    }
}

// ── workspace ───────────────────────────────────────────────────────────────

fn workspace(name: &str) -> merlin::workspace::Workspace {
    merlin::workspace::Workspace::new(scratch(name)).unwrap()
}

#[test]
fn an_edit_applies_only_when_every_replacement_is_unambiguous() {
    let w = workspace("ws-edit");
    w.write("a.txt", "alpha\nbeta\ngamma\nbeta\n").unwrap();

    // "beta" appears twice, so the edit cannot know which was meant.
    let ambiguous = w.edit(
        "a.txt",
        &[merlin::workspace::Edit {
            old: "beta".into(),
            new: "delta".into(),
        }],
    );
    assert!(ambiguous.is_err(), "an ambiguous edit must be refused");
    assert_eq!(
        std::fs::read_to_string(w.root().join("a.txt")).unwrap(),
        "alpha\nbeta\ngamma\nbeta\n",
        "a refused edit must not have written anything"
    );

    // A failing edit in a batch rolls the whole batch back, so the file never
    // ends up half-edited.
    let partial = w.edit(
        "a.txt",
        &[
            merlin::workspace::Edit {
                old: "alpha".into(),
                new: "ALPHA".into(),
            },
            merlin::workspace::Edit {
                old: "nowhere".into(),
                new: "x".into(),
            },
        ],
    );
    assert!(partial.is_err());
    assert_eq!(
        std::fs::read_to_string(w.root().join("a.txt")).unwrap(),
        "alpha\nbeta\ngamma\nbeta\n",
        "no edit may land if any edit in the call fails"
    );

    w.edit(
        "a.txt",
        &[merlin::workspace::Edit {
            old: "alpha\nbeta".into(),
            new: "alpha\nBETA".into(),
        }],
    )
    .unwrap();
    assert!(
        std::fs::read_to_string(w.root().join("a.txt"))
            .unwrap()
            .contains("BETA")
    );
}

#[test]
fn paths_cannot_climb_out_of_the_workspace() {
    let w = workspace("ws-escape");
    for attempt in ["../escaped.txt", "../../etc/passwd", "a/../../../tmp/x"] {
        assert!(
            w.write(attempt, "nope").is_err(),
            "{attempt} should have been refused"
        );
    }
    // A leading slash is treated as workspace-relative rather than as the host root.
    w.write("/inside.txt", "fine").unwrap();
    assert!(w.root().join("inside.txt").exists());
}
