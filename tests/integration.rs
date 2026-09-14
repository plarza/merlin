use merlin::commands::Command;
use merlin::config::Config;
use merlin::cron::{self, Job};
use merlin::embed::{self, Memories, Messages};
use merlin::exec::Sandbox;
use merlin::llm::{Attachment, Llm, Message};
use merlin::room::{Buffers, Turn, is_addressed};
use merlin::tools::definitions;
use merlin::workspace::Workspace;
use merlin::{db, memory, messages};
use rusqlite::Connection;

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

fn database(name: &str) -> Connection {
    db::open(&scratch(name).join("merlin.db")).unwrap()
}

#[test]
fn workspaces_are_scoped_by_room() {
    let workspace = Workspace::new(scratch("room-workspaces")).unwrap();
    let a = workspace.scoped("!a:x").unwrap();
    let b = workspace.scoped("!b:x").unwrap();
    a.write("note.txt", "alpha").unwrap();
    b.write("note.txt", "beta").unwrap();

    assert_ne!(a.root(), b.root());
    assert_eq!(
        std::fs::read_to_string(a.root().join("note.txt")).unwrap(),
        "alpha"
    );
    assert_eq!(
        std::fs::read_to_string(b.root().join("note.txt")).unwrap(),
        "beta"
    );
}

#[test]
fn legacy_data_is_split_into_physical_room_databases() {
    let state = scratch("room-databases");
    let legacy = db::open(&state.join("merlin.db")).unwrap();
    memory::store(
        &legacy,
        "legacy",
        "belongs to the original room",
        "core",
        None,
    )
    .unwrap();
    memory::store(&legacy, "private-b", "only room b", "core", Some("!b:x")).unwrap();
    messages::record(
        &legacy,
        "$a",
        "!a:x",
        "@a:x",
        "alpha",
        "2026-01-01T00:00:00Z",
    )
    .unwrap();
    messages::record(
        &legacy,
        "$b",
        "!b:x",
        "@b:x",
        "beta",
        "2026-01-01T00:00:00Z",
    )
    .unwrap();
    drop(legacy);

    let rooms = db::RoomDbs::open(&state, &["!a:x".into(), "!b:x".into()]).unwrap();
    let a = rooms.get("!a:x").unwrap();
    let b = rooms.get("!b:x").unwrap();

    assert_eq!(messages::count(&a.lock().unwrap()).unwrap(), 1);
    assert_eq!(messages::count(&b.lock().unwrap()).unwrap(), 1);
    assert_eq!(memory::count(&a.lock().unwrap()).unwrap(), 1);
    assert_eq!(memory::count(&b.lock().unwrap()).unwrap(), 1);
    assert!(state.join("merlin.db.migrated").exists());
    assert_ne!(
        db::room_key("!a:x"),
        db::room_key("!b:x"),
        "room database filenames must not collide"
    );
}

#[test]
fn slash_commands_must_be_the_whole_message() {
    assert_eq!(Command::parse("/help"), Some(Command::Help));
    assert_eq!(Command::parse("/model"), Some(Command::Model(None)));
    assert_eq!(
        Command::parse("/model openai/gpt-5.6-sol"),
        Some(Command::Model(Some("openai/gpt-5.6-sol")))
    );
    assert_eq!(Command::parse("/reasoning"), Some(Command::Reasoning(None)));
    assert_eq!(
        Command::parse("/reasoning high"),
        Some(Command::Reasoning(Some("high")))
    );

    for invalid in [
        "/model ",
        "/model  openai/gpt-5.6-sol",
        "/model openai/gpt-5.6-sol please",
        "merlin /model openai/gpt-5.6-sol",
        "/model openai/gpt-5.6-sol\n",
        "/help please",
        "/reasoning ",
        "/reasoning very high",
    ] {
        assert_eq!(Command::parse(invalid), None, "accepted {invalid:?}");
    }
}

#[test]
fn only_configured_admins_are_administrators() {
    let path = scratch("admins").join("config.toml");
    std::fs::write(
        &path,
        r#"
homeserver = "https://matrix.example.org"
user_id = "@merlin:matrix.example.org"
display_name = "merlin"
allowed_rooms = ["!room:example.org"]
allowed_senders = ["@aiden:example.org", "@atlas:example.org"]
admin_senders = ["@aiden:example.org"]
untrusted_senders = ["@atlas:example.org"]
"#,
    )
    .unwrap();

    let config = Config::load(&path).unwrap();
    assert!(config.is_admin("@aiden:example.org"));
    assert!(!config.is_admin("@atlas:example.org"));
    assert_eq!(
        config.trust_level("@aiden:example.org"),
        merlin::config::TrustLevel::Admin
    );
    assert_eq!(
        config.trust_level("@atlas:example.org"),
        merlin::config::TrustLevel::Untrusted
    );
}

#[test]
fn trust_roles_must_be_consistent_with_access_roles() {
    for (name, untrusted) in [
        ("not-allowed", "@stranger:example.org"),
        ("also-admin", "@aiden:example.org"),
    ] {
        let path = scratch(name).join("config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
homeserver = "https://matrix.example.org"
user_id = "@merlin:matrix.example.org"
display_name = "merlin"
allowed_rooms = ["!room:example.org"]
allowed_senders = ["@aiden:example.org", "@atlas:example.org"]
admin_senders = ["@aiden:example.org"]
untrusted_senders = ["{untrusted}"]
"#
            ),
        )
        .unwrap();

        assert!(Config::load(&path).is_err());
    }
}

#[test]
fn switching_model_returns_the_previous_slug() {
    let llm = Llm::new("key".into(), "old/model".into(), "medium".into(), 10).unwrap();

    assert_eq!(llm.switch_model("new/model"), "old/model");
    assert_eq!(llm.model(), "new/model");
    assert_eq!(llm.switch_reasoning_effort("high"), "medium");
    assert_eq!(llm.reasoning_effort(), "high");
}

#[test]
fn memory_survives_reopening_and_recalls_by_keyword() {
    let path = scratch("memory").join("merlin.db");

    {
        let db = db::open(&path).unwrap();
        memory::store(
            &db,
            "kettle",
            "the kettle in the kitchen is broken",
            "core",
            None,
        )
        .unwrap();
        memory::store(
            &db,
            "parking",
            "visitor parking is free after six",
            "core",
            None,
        )
        .unwrap();
        memory::store(
            &db,
            "kettle",
            "the kettle is broken. a new one is on order",
            "core",
            None,
        )
        .unwrap();
    }

    let db = db::open(&path).unwrap();
    assert_eq!(
        memory::count(&db).unwrap(),
        2,
        "upsert must not duplicate a key"
    );

    let hits = memory::recall(&db, "kettle broken", None, 5).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(
        hits[0].content.contains("on order"),
        "the revision must win"
    );

    assert!(memory::recall(&db, "!!!", None, 5).is_ok());
}

fn seeded_archive(name: &str) -> Connection {
    let db = database(name);
    for (i, (sender, body)) in [
        ("@sam:x", "the shoelace snapped on my boot"),
        ("@lee:x", "world cup final was in 2025"),
        ("@kim:x", "the 2018 final went to penalties"),
        ("@sam:x", "a gymnast stuck the landing"),
    ]
    .iter()
    .enumerate()
    {
        messages::record(
            &db,
            &format!("$e{i}"),
            "!r:x",
            sender,
            body,
            &format!("2026-01-0{}T00:00:00Z", i + 1),
        )
        .unwrap();
    }
    db
}

#[test]
fn bare_words_tolerate_misspelling() {
    let db = seeded_archive("fuzzy");
    let hits = messages::search(&db, "shoelase", None, 5).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].body.contains("shoelace"));
}

#[test]
fn quoted_terms_are_required_and_combine_with_fuzzy_ones() {
    let db = seeded_archive("quoted");

    let hits = messages::search(&db, "wrold cup \"2025\"", None, 5).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].body.contains("2025"));

    let hits = messages::search(&db, "finl \"2018\"", None, 5).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].body.contains("2018"));

    assert_eq!(
        messages::search(&db, "\"gymnast\"", None, 5).unwrap().len(),
        1
    );
    assert!(
        messages::search(&db, "\"gymnasts\"", None, 5)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn search_rejects_nothing_and_finds_nothing_for_unrelated_queries() {
    let db = seeded_archive("empty");
    assert!(messages::search(&db, "s&p", None, 5).is_ok());
    assert!(messages::search(&db, "???", None, 5).is_ok());
    assert!(messages::search(&db, "", None, 5).unwrap().is_empty());
    assert!(
        messages::search(&db, "elephant", None, 5)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn recent_returns_the_newest_messages_oldest_first() {
    let db = seeded_archive("recent");
    let rows = messages::recent(&db, "!r:x", 2).unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows[0].body.contains("2018"));
    assert!(rows[1].body.contains("gymnast"));
    assert!(messages::recent(&db, "!other:x", 5).unwrap().is_empty());
}

#[test]
fn an_event_is_archived_once_however_often_sync_replays_it() {
    let db = database("replay");
    for _ in 0..3 {
        messages::record(
            &db,
            "$same",
            "!r:x",
            "@sam:x",
            "hello",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
    }
    assert_eq!(messages::count(&db).unwrap(), 1);
}

#[test]
fn the_three_old_databases_are_folded_into_one() {
    let dir = scratch("migrate");

    {
        let old = db::open(&dir.join("memory.db")).unwrap();
        memory::store(&old, "kept", "this must survive", "core", None).unwrap();
    }
    {
        let old = db::open(&dir.join("messages.db")).unwrap();
        messages::record(
            &old,
            "$m1",
            "!r:x",
            "@sam:x",
            "so must this",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
    }
    {
        let old = db::open(&dir.join("cron.db")).unwrap();
        cron::upsert(&old, &job("nightly", "0 7 * * *")).unwrap();
    }

    let mut db = db::open(&dir.join("merlin.db")).unwrap();
    let moved = db::migrate_from_split_files(&mut db, &dir).unwrap();
    assert_eq!(moved, 3, "one row from each file");

    assert_eq!(memory::count(&db).unwrap(), 1);
    assert_eq!(messages::count(&db).unwrap(), 1);
    assert_eq!(cron::list(&db).unwrap().len(), 1);

    assert!(dir.join("memory.db.migrated").exists());
    assert!(!dir.join("memory.db").exists());
    assert_eq!(db::migrate_from_split_files(&mut db, &dir).unwrap(), 0);

    assert_eq!(memory::recall(&db, "survive", None, 5).unwrap().len(), 1);
    assert_eq!(messages::search(&db, "\"must\"", None, 5).unwrap().len(), 1);
}

#[test]
fn sql_queries_can_read_everything_and_write_nothing() {
    let db = database("sql");
    memory::store(&db, "a", "first note", "core", None).unwrap();
    memory::store(&db, "b", "second note", "daily", None).unwrap();
    messages::record(&db, "$1", "!r:x", "@sam:x", "hello", "2026-01-01T00:00:00Z").unwrap();

    let out = db::query(
        &db,
        "SELECT category, count(*) FROM memories GROUP BY category",
        50,
    )
    .unwrap();
    assert!(out.contains("core | 1"), "got: {out}");
    assert!(out.contains("daily | 1"), "got: {out}");

    let out = db::query(
        &db,
        "SELECT (SELECT count(*) FROM memories) AS m, (SELECT count(*) FROM messages) AS n",
        50,
    )
    .unwrap();
    assert!(out.contains("2 | 1"), "got: {out}");

    for write in [
        "DELETE FROM memories",
        "UPDATE memories SET content = 'x'",
        "INSERT INTO memories (id, key, content, category, created_at, updated_at) VALUES ('i','k','c','core','t','t')",
        "DROP TABLE memories",
    ] {
        assert!(
            db::query(&db, write, 50).is_err(),
            "{write} must be refused"
        );
    }
    assert_eq!(
        memory::count(&db).unwrap(),
        2,
        "nothing may have been written"
    );

    assert!(
        db::query(&db, "SELECT * FROM memories WHERE 0", 50)
            .unwrap()
            .contains("No rows")
    );
    assert!(db::query(&db, "SELECT nonsense syntax(", 50).is_err());

    assert!(
        db::query(&db, "ATTACH DATABASE '/tmp/other.db' AS other", 50).is_err(),
        "attaching another database must be refused"
    );
}

fn vector(x: f32, y: f32, z: f32) -> Vec<f32> {
    vec![x, y, z]
}

const MODEL: &str = "test/embedding-model";

fn embed_all<E: embed::Embeddable>(db: &mut Connection, pick: impl Fn(&str) -> Vec<f32>) {
    let pending = embed::pending::<E>(db, 100).unwrap();
    let rows: Vec<(i64, Vec<f32>)> = pending.iter().map(|(id, text)| (*id, pick(text))).collect();
    embed::save::<E>(db, &rows).unwrap();
}

#[test]
fn semantic_recall_ranks_memories_by_direction_not_magnitude() {
    let mut db = database("semantic-memory");
    embed::ensure_table::<Memories>(&db, MODEL, 3).unwrap();

    memory::store(&db, "boiler", "the heating packed up", "core", None).unwrap();
    memory::store(
        &db,
        "parking",
        "visitor parking is free after six",
        "core",
        None,
    )
    .unwrap();
    assert_eq!(
        embed::pending::<Memories>(&db, 10).unwrap().len(),
        2,
        "nothing is embedded until the loop runs"
    );

    embed_all::<Memories>(&mut db, |text| {
        if text.contains("heating") {
            vector(0.31, 0.0, 0.0)
        } else {
            vector(0.0, 1.0, 0.0)
        }
    });
    assert!(embed::pending::<Memories>(&db, 10).unwrap().is_empty());

    let hits = memory::recall(&db, "heating trouble", Some(&vector(9.7, 0.0, 0.0)), 2).unwrap();
    assert_eq!(hits[0].key, "boiler");
    assert_eq!(hits[1].key, "parking");
}

#[test]
fn revising_a_memory_re_embeds_it() {
    let mut db = database("semantic-revise");
    embed::ensure_table::<Memories>(&db, MODEL, 3).unwrap();

    memory::store(&db, "kettle", "the kettle is broken", "core", None).unwrap();
    embed_all::<Memories>(&mut db, |_| vector(1.0, 0.0, 0.0));
    assert!(embed::pending::<Memories>(&db, 10).unwrap().is_empty());

    memory::store(
        &db,
        "kettle",
        "the kettle was replaced on tuesday",
        "core",
        None,
    )
    .unwrap();
    assert_eq!(
        embed::pending::<Memories>(&db, 10).unwrap().len(),
        1,
        "revised content must be queued for re-embedding"
    );
}

#[test]
fn forgetting_a_memory_takes_its_vector_with_it() {
    let mut db = database("semantic-forget");
    embed::ensure_table::<Memories>(&db, MODEL, 3).unwrap();

    memory::store(&db, "doomed", "this will be deleted", "core", None).unwrap();
    embed_all::<Memories>(&mut db, |_| vector(1.0, 0.0, 0.0));
    memory::forget(&db, "doomed").unwrap();

    let hits = memory::recall(&db, "deleted thing", Some(&vector(1.0, 0.0, 0.0)), 5).unwrap();
    assert!(
        hits.iter().all(|h| h.key != "doomed"),
        "a deleted memory must not still be reachable by meaning, found {hits:?}"
    );

    memory::store(&db, "fresh", "something else entirely", "core", None).unwrap();
    assert_eq!(
        embed::pending::<Memories>(&db, 10).unwrap().len(),
        1,
        "the reused rowid must not inherit a vector"
    );
}

#[test]
fn changing_the_embedding_model_discards_incompatible_vectors() {
    let mut db = database("semantic-model-swap");
    embed::ensure_table::<Memories>(&db, MODEL, 3).unwrap();
    memory::store(&db, "a", "some note", "core", None).unwrap();
    embed_all::<Memories>(&mut db, |_| vector(1.0, 0.0, 0.0));
    assert!(embed::pending::<Memories>(&db, 10).unwrap().is_empty());

    embed::ensure_table::<Memories>(&db, "test/other-model", 4).unwrap();
    assert_eq!(
        embed::pending::<Memories>(&db, 10).unwrap().len(),
        1,
        "vectors from another model must be rebuilt, not reused"
    );
}

#[test]
fn quoted_terms_filter_and_the_rest_ranks_by_meaning() {
    let mut db = database("semantic-combined");
    embed::ensure_table::<Messages>(&db, MODEL, 3).unwrap();

    for (i, (sender, body)) in [
        ("@sam:x", "the world cup final was in 2025"),
        ("@lee:x", "quarterly revenue for 2025 was strong"),
        ("@kim:x", "the world cup was thrilling"),
    ]
    .iter()
    .enumerate()
    {
        messages::record(
            &db,
            &format!("${i}"),
            "!r:x",
            sender,
            body,
            &format!("2026-01-0{}T00:00:00Z", i + 1),
        )
        .unwrap();
    }
    embed_all::<Messages>(&mut db, |text| {
        if text.contains("world cup") {
            vector(1.0, 0.0, 0.0)
        } else {
            vector(0.0, 1.0, 0.0)
        }
    });

    let bodies: Vec<String> =
        messages::search(&db, "world cup \"2025\"", Some(&vector(1.0, 0.0, 0.0)), 5)
            .unwrap()
            .into_iter()
            .map(|h| h.body)
            .collect();

    assert!(
        !bodies.iter().any(|b| b == "the world cup was thrilling"),
        "a message without the required term must be excluded however well it matches in meaning, got {bodies:?}"
    );
    assert_eq!(
        bodies,
        vec![
            "the world cup final was in 2025".to_string(),
            "quarterly revenue for 2025 was strong".to_string()
        ],
        "both keep 2025; the world cup one ranks first on meaning"
    );
}

#[test]
fn an_all_quoted_query_stays_exact() {
    let mut db = database("semantic-quoted-only");
    embed::ensure_table::<Messages>(&db, MODEL, 3).unwrap();
    messages::record(
        &db,
        "$a",
        "!r:x",
        "@sam:x",
        "the invoice went out friday",
        "2026-01-01T00:00:00Z",
    )
    .unwrap();
    messages::record(
        &db,
        "$b",
        "!r:x",
        "@lee:x",
        "lunch was excellent",
        "2026-01-02T00:00:00Z",
    )
    .unwrap();
    embed_all::<Messages>(&mut db, |_| vector(1.0, 0.0, 0.0));

    let hits = messages::search(&db, "\"invoice\"", None, 5).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].sender, "@sam:x");
}

#[test]
fn search_falls_back_to_matching_text_when_nothing_is_embedded_yet() {
    let db = database("semantic-backfilling");
    embed::ensure_table::<Messages>(&db, MODEL, 3).unwrap();
    messages::record(
        &db,
        "$a",
        "!r:x",
        "@sam:x",
        "the shoelace snapped",
        "2026-01-01T00:00:00Z",
    )
    .unwrap();

    let hits = messages::search(&db, "shoelase", Some(&vector(1.0, 0.0, 0.0)), 5).unwrap();
    assert_eq!(hits.len(), 1, "the fallback must still answer");
    assert_eq!(hits[0].body, "the shoelace snapped");
}

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

    assert!(!addressed("merlinesque behaviour"));
    assert!(!addressed("submerlin"));
    assert!(!addressed("what do you reckon about the game"));
    assert!(!addressed(""));

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
fn an_explicit_mention_addresses_the_bot_without_naming_it() {
    assert!(is_addressed(
        "can you look at this",
        &[UID.into()],
        UID,
        "merlin",
        "merlin",
        false
    ));
}

#[test]
fn someone_elses_mention_does_not_suppress_the_name() {
    // What a reply looks like: Element attaches an m.mentions for the person
    // being replied to, which used to make the bot ignore its own name.
    assert!(is_addressed(
        "merlin do a security review",
        &["@bob:example.org".to_string()],
        UID,
        "merlin",
        "merlin",
        false
    ));

    // Still nothing to answer when the name is absent.
    assert!(!is_addressed(
        "bob can you look at this",
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

    assert_eq!(b.render("!a", true).unwrap(), "@x: 2\n@x: 3");
    assert_eq!(b.render("!b", false).unwrap(), "@y: other room");
    assert!(b.render("!missing", false).is_none());
}

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
    let path = scratch("cron").join("merlin.db");

    {
        let db = db::open(&path).unwrap();
        cron::upsert(&db, &job("hn", "0 7 * * *")).unwrap();
        cron::upsert(&db, &job("hn", "0 8 * * *")).unwrap();
        cron::record_run(&db, "hn", "ok").unwrap();
    }

    let db = db::open(&path).unwrap();
    let jobs = cron::list(&db).unwrap();
    assert_eq!(jobs.len(), 1, "same name edits rather than duplicates");
    assert_eq!(jobs[0].schedule, "0 8 * * *");

    assert!(cron::delete(&db, "hn").unwrap());
    assert!(!cron::delete(&db, "hn").unwrap());
}

#[test]
fn crontab_syntax_is_accepted_and_nonsense_is_refused() {
    assert!(job("hn", "0 7 * * *").validate().is_ok());
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
    let out = sb.run("echo 42", "!room-a:example.org").await.unwrap();
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
    let out = sb.run("true", "!room-a:example.org").await.unwrap();
    assert!(out.timed_out);
    assert!(out.stderr.contains("killed"));
}

#[test]
fn tool_results_and_calls_match_the_openai_wire_format() {
    let result = serde_json::to_value(Message::tool_result("call_1", "42")).unwrap();
    assert_eq!(result["role"], "tool");
    assert_eq!(result["tool_call_id"], "call_1");
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
    let url = parts[1]["image_url"]["url"].as_str().unwrap();
    assert!(url.starts_with("data:image/png;base64,"));

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
        "bash",
        "cron_create",
        "sql_query",
        "cron_delete",
    ] {
        assert!(names.contains(&expected.to_string()), "missing {expected}");
    }

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

fn workspace(name: &str) -> merlin::workspace::Workspace {
    merlin::workspace::Workspace::new(scratch(name)).unwrap()
}

#[test]
fn an_edit_applies_only_when_every_replacement_is_unambiguous() {
    let w = workspace("ws-edit");
    w.write("a.txt", "alpha\nbeta\ngamma\nbeta\n").unwrap();

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
    w.write("/inside.txt", "fine").unwrap();
    assert!(w.root().join("inside.txt").exists());
}

#[test]
fn paths_cannot_leave_through_a_symlink_the_sandbox_planted() {
    let w = workspace("ws-symlink");
    let outside = scratch("ws-symlink-outside");
    std::os::unix::fs::symlink(&outside, w.root().join("escape")).unwrap();

    assert!(w.write("escape/loot.txt", "nope").is_err());
    assert!(w.write("escape", "nope").is_err());
    assert!(!outside.join("loot.txt").exists());
}

#[test]
fn fetches_are_refused_when_the_host_is_not_public() {
    for attempt in [
        "http://127.0.0.1:5000/v2/_catalog",
        "http://10.0.0.5/",
        "http://192.168.1.1/",
        "http://169.254.169.254/latest/meta-data/",
        "http://[::1]:3001/",
        "file:///etc/passwd",
    ] {
        assert!(
            merlin::tools::public_url(attempt).is_err(),
            "{attempt} should have been refused"
        );
    }
}

#[tokio::test]
async fn the_run_limits_reach_the_sandbox_as_arguments() {
    use std::os::unix::fs::PermissionsExt;

    let script = scratch("exec-args").join("record");
    std::fs::write(
        &script,
        "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let sb = Sandbox::new(vec![script.display().to_string()], 30, "1G".into());
    let out = sb.run("echo hello", "!room-a:example.org").await.unwrap();

    assert_eq!(
        out.stdout.lines().collect::<Vec<_>>(),
        vec![
            "30",
            "1048576",
            merlin::db::room_key("!room-a:example.org").as_str()
        ],
        "timeout, address-space limit and opaque room scope"
    );
}

#[test]
fn a_tool_call_is_logged_with_enough_to_identify_it() {
    let line = merlin::agent::summarise(&serde_json::json!({
        "url": "https://example.com/thing",
        "limit": 8
    }));
    assert!(
        line.contains("url=https://example.com/thing"),
        "got: {line}"
    );
    assert!(line.contains("limit=8"), "got: {line}");

    let long = merlin::agent::summarise(&serde_json::json!({
        "script": format!("echo one\n{}", "x".repeat(4000))
    }));
    assert!(
        long.len() < 700,
        "a long argument must be cut: {}",
        long.len()
    );
    assert!(
        !long.contains('\n'),
        "a logged argument must stay on one line"
    );

    let query = format!(
        "WITH RECURSIVE split(sender, w, rest) AS (SELECT sender, '', lower(body) || ' ' \
         FROM messages UNION ALL SELECT sender, substr(rest, 1, instr(rest, ' ') - 1), \
         substr(rest, instr(rest, ' ') + 1) FROM split WHERE rest <> '')\n{}",
        "SELECT w FROM split;"
    );
    let logged = merlin::agent::summarise(&serde_json::json!({ "query": query.clone() }));
    assert!(
        logged.contains("SELECT w FROM split;"),
        "a realistic query must survive whole: {logged}"
    );
}

#[test]
fn choosing_fal_without_a_key_fails_at_startup() {
    let err = match merlin::image::ImageGen::new(
        merlin::image::Provider::Fal,
        "fal-ai/z-image/turbo".into(),
        "openrouter-key".into(),
        None,
        60,
    ) {
        Ok(_) => panic!("fal with no key must not construct"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("FAL_API_KEY"), "got: {err}");

    assert!(
        merlin::image::ImageGen::new(
            merlin::image::Provider::OpenRouter,
            "meta/muse-image".into(),
            "openrouter-key".into(),
            None,
            60,
        )
        .is_ok()
    );
}
