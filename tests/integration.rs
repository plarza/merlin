//! End to end tests against real databases and real processes.
//!
//! Everything here goes through the public API on real files,
//! so a passing run means the SQLite schema,
//! the FTS indexes and the process plumbing actually work,
//! not that a helper returns what it was told to.

use merlin::cron::{CronStore, Job};
use merlin::exec::Sandbox;
use merlin::llm::Message;
use merlin::memory::Memory;
use merlin::messages::Archive;
use merlin::room::{Buffers, Turn, is_addressed};
use merlin::tools::definitions;

/// A unique directory per test,
/// so runs do not share state.
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
        m.store("zog", "Zog is an alien with his own file", "core", None)
            .unwrap();
        m.store(
            "john",
            "John Carroll is an Australian gymnast",
            "core",
            None,
        )
        .unwrap();
        // Re-filing the same subject revises it.
        m.store("zog", "Zog is an alien. John hates him", "core", None)
            .unwrap();
    }

    let m = Memory::open(&path).unwrap();
    assert_eq!(m.count().unwrap(), 2, "upsert must not duplicate a key");

    let hits = m.recall("zog alien", 5).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(
        hits[0].content.contains("John hates him"),
        "kept the revision"
    );

    // A query that FTS5 would reject as syntax must still be answerable.
    assert!(m.recall("!!!", 5).is_ok());

    assert!(m.forget("zog").unwrap());
    assert!(!m.forget("zog").unwrap());
    assert_eq!(m.count().unwrap(), 1);
}

// ── message search ──────────────────────────────────────────────────────────

fn seeded_archive(dir: &std::path::Path) -> Archive {
    let a = Archive::open(&dir.join("messages.db")).unwrap();
    for (i, (sender, body)) in [
        ("@aiden:x.org", "file the zog shoelace incident"),
        ("@jakob:y.org", "john carroll is an australian gymnast"),
        ("@aiden:x.org", "who won the fifa 2025 world cup"),
        ("@jakob:y.org", "the fifa 2018 world cup was in russia"),
    ]
    .iter()
    .enumerate()
    {
        a.record(
            &format!("$e{i}"),
            "!r:x.org",
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
    let hits = seeded_archive(&dir).search("shoelase", 5).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].body.contains("shoelace"));
}

#[test]
fn quoted_terms_are_required_and_combine_with_fuzzy_ones() {
    let dir = scratch("quoted");
    let a = seeded_archive(&dir);

    // Both messages concern the world cup; the quoted year selects one.
    let hits = a.search("fifa \"2025\" world cup", 5).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].body.contains("2025"));

    // A misspelled loose term alongside a required one.
    let hits = a.search("wrold \"2018\"", 5).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].body.contains("2018"));

    // Quoting alone is an exact search,
    // so a near miss finds nothing.
    assert_eq!(a.search("\"gymnast\"", 5).unwrap().len(), 1);
    assert!(a.search("\"gymnasts\"", 5).unwrap().is_empty());
}

#[test]
fn search_rejects_nothing_and_finds_nothing_for_unrelated_queries() {
    let dir = scratch("edges");
    let a = seeded_archive(&dir);
    assert!(a.search("s&p", 5).is_ok());
    assert!(a.search("???", 5).is_ok());
    assert!(a.search("", 5).unwrap().is_empty());
    assert!(a.search("elephant", 5).unwrap().is_empty());
}

#[test]
fn an_event_is_archived_once_however_often_sync_replays_it() {
    let dir = scratch("dupes");
    let a = seeded_archive(&dir);
    let before = a.count().unwrap();
    a.record("$e0", "!r:x.org", "@aiden:x.org", "different text", "now")
        .unwrap();
    assert_eq!(a.count().unwrap(), before);
}

// ── addressing ──────────────────────────────────────────────────────────────

const UID: &str = "@merlin:matrix.aza.network";

fn addressed(body: &str) -> bool {
    is_addressed(body, &[], UID, "merlin", "merlin", false)
}

#[test]
fn a_turn_starts_only_when_the_bot_is_actually_addressed() {
    assert!(addressed("merlin what day is it"));
    assert!(addressed("hey Merlin, you there?"));
    assert!(addressed("@merlin hello"));
    assert!(addressed("ask @merlin:matrix.aza.network about it"));

    // Word boundary,
    // so the bird and the wizard do not wake it.
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
        &["@jakob:sadairs.com".to_string()],
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
        room_id: "!r:x.org".into(),
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
            "function": { "name": "memory_recall", "arguments": "{\"query\":\"zog\"}" }
        }]
    }))
    .unwrap();
    assert_eq!(parsed.tool_calls[0].function.name, "memory_recall");
}

#[test]
fn every_tool_the_agent_is_offered_is_described() {
    let names: Vec<String> = definitions()
        .iter()
        .map(|d| {
            let f = &d["function"];
            assert_eq!(f["parameters"]["type"], "object");
            // The description is the only thing telling the model when to reach for a tool,
            // so an empty one is a real defect.
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
        "http_request",
        "generate_image",
        "run_code",
        "cron_create",
        "cron_list",
        "cron_delete",
        "time_now",
    ] {
        assert!(names.contains(&expected.to_string()), "missing {expected}");
    }
}
