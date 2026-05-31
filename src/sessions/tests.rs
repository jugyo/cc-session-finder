use rusqlite::{params, Connection};

use super::*;
use crate::index::schema;

fn setup() -> Connection {
    let conn = Connection::open_in_memory().expect("open");
    schema::ensure(&conn).expect("schema");
    conn
}

#[allow(clippy::too_many_arguments)]
fn insert_session(
    conn: &Connection,
    session_id: &str,
    agent: &str,
    mtime: i64,
    ai_title: Option<&str>,
    first_prompt: Option<&str>,
    git_branch: Option<&str>,
) {
    conn.execute(
        "INSERT INTO sessions
           (session_id, agent, native_session_id, cwd, ai_title, first_prompt,
            mtime, size, file_path, git_branch, tokens_input, tokens_output)
         VALUES (?1, ?2, ?3, '/repo', ?4, ?5, ?6, 0, '/f.jsonl', ?7, 100, 50)",
        params![
            session_id,
            agent,
            format!("native-{session_id}"),
            ai_title,
            first_prompt,
            mtime,
            git_branch
        ],
    )
    .expect("insert session");
}

fn insert_msg(conn: &Connection, session_id: &str, turn_index: u32, role: &str, text: &str) {
    conn.execute(
        "INSERT INTO messages (session_id, turn_index, role, text) VALUES (?1, ?2, ?3, ?4)",
        params![session_id, turn_index as i64, role, text],
    )
    .expect("insert message");
}

fn seed(conn: &Connection) {
    insert_session(
        conn,
        "s1",
        "claude",
        2000,
        Some("First"),
        Some("hello there"),
        Some("main"),
    );
    insert_msg(conn, "s1", 0, "user", "hello there alpha topic");
    insert_msg(conn, "s1", 1, "assistant", "sure, the alpha answer");
    insert_msg(conn, "s1", 2, "user", "more about beta now");
    insert_msg(conn, "s1", 3, "assistant", "final beta wrapup");

    insert_session(
        conn,
        "codex:s2",
        "codex",
        1000,
        Some("Second"),
        Some("start"),
        None,
    );
    insert_msg(conn, "codex:s2", 0, "user", "unrelated gamma chatter");
}

#[test]
fn recent_list_has_recent_reason_and_latest_message() {
    let conn = setup();
    seed(&conn);

    let resp = search_sessions(&conn, SearchParams::default()).unwrap();

    assert_eq!(resp.count, 2);
    // Newest mtime first.
    let card = &resp.results[0];
    assert_eq!(card.id, "s1");
    assert_eq!(card.match_reasons, vec!["recent".to_string()]);
    assert!(card.matches.is_empty());
    let latest = card.latest_message.as_ref().expect("latest");
    assert_eq!(latest.message_index, 3);
    assert_eq!(latest.role, "assistant");
    assert_eq!(card.metadata.message_count, 4);
    assert_eq!(card.metadata.tokens_total, 150);
    assert_eq!(card.metadata.git_branch.as_deref(), Some("main"));
}

#[test]
fn search_returns_message_matches_with_index() {
    let conn = setup();
    seed(&conn);

    let resp = search_sessions(
        &conn,
        SearchParams {
            query: Some("alpha".to_string()),
            ..Default::default()
        },
    )
    .unwrap();

    let card = resp.results.iter().find(|c| c.id == "s1").expect("s1");
    assert_eq!(card.match_reasons, vec!["message".to_string()]);
    assert!(!card.matches.is_empty());
    assert!(card
        .matches
        .iter()
        .all(|m| m.role == "user" || m.role == "assistant"));
    assert!(card
        .matches
        .iter()
        .any(|m| m.message_index == 0 || m.message_index == 1));
}

#[test]
fn overview_returns_first_latest_and_count() {
    let conn = setup();
    seed(&conn);

    let resp = get_session_overview(&conn, "s1").unwrap().expect("found");
    assert_eq!(resp.session.id, "s1");
    assert_eq!(resp.message_count, 4);
    assert_eq!(resp.first_message.as_ref().unwrap().message_index, 0);
    assert_eq!(resp.latest_message.as_ref().unwrap().message_index, 3);

    assert!(get_session_overview(&conn, "missing").unwrap().is_none());
}

#[test]
fn messages_paginate_with_order_and_bounds() {
    let conn = setup();
    seed(&conn);

    let asc = get_session_messages(
        &conn,
        MessagesParams {
            id: "s1".to_string(),
            limit: Some(2),
            order: MessageOrder::Asc,
            after_message_index: Some(0),
            before_message_index: None,
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        asc.messages
            .iter()
            .map(|m| m.message_index)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert!(asc.has_more_before); // index 0 exists before 1
    assert!(asc.has_more_after); // index 3 exists after 2

    let desc = get_session_messages(
        &conn,
        MessagesParams {
            id: "s1".to_string(),
            limit: Some(10),
            order: MessageOrder::Desc,
            after_message_index: None,
            before_message_index: None,
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        desc.messages
            .iter()
            .map(|m| m.message_index)
            .collect::<Vec<_>>(),
        vec![3, 2, 1, 0]
    );
    assert!(!desc.has_more_before);
    assert!(!desc.has_more_after);
}

#[test]
fn messages_limit_is_capped() {
    let conn = setup();
    insert_session(&conn, "big", "claude", 1, None, None, None);
    for i in 0..40u32 {
        insert_msg(&conn, "big", i, "user", &format!("line {i}"));
    }

    let resp = get_session_messages(
        &conn,
        MessagesParams {
            id: "big".to_string(),
            limit: Some(1000),
            order: MessageOrder::Asc,
            after_message_index: None,
            before_message_index: None,
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(resp.messages.len(), 30); // MESSAGES_LIMIT_CAP
}

#[test]
fn session_local_search_scopes_to_one_session() {
    let conn = setup();
    seed(&conn);

    let resp = search_session_messages(&conn, "s1", "beta", None)
        .unwrap()
        .unwrap();
    assert_eq!(resp.session.id, "s1");
    assert!(resp.count >= 1);
    assert!(resp
        .matches
        .iter()
        .all(|m| m.message_index == 2 || m.message_index == 3));

    // "gamma" only exists in codex:s2, not in s1.
    let none_in_s1 = search_session_messages(&conn, "s1", "gamma", None)
        .unwrap()
        .unwrap();
    assert_eq!(none_in_s1.count, 0);

    assert!(search_session_messages(&conn, "missing", "beta", None)
        .unwrap()
        .is_none());
}

#[test]
fn no_native_or_debug_fields_leak_in_json() {
    let conn = setup();
    seed(&conn);

    let resp = search_sessions(
        &conn,
        SearchParams {
            query: Some("alpha".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    let json = serde_json::to_string(&resp).unwrap();

    for forbidden in [
        "native_session_id",
        "native-",
        "file_path",
        "scores",
        "is_worktree",
        "source_group",
        "snippet",
        "/f.jsonl",
    ] {
        assert!(
            !json.contains(forbidden),
            "forbidden token {forbidden:?} leaked into MCP JSON"
        );
    }

    let overview = get_session_overview(&conn, "s1").unwrap().unwrap();
    let json = serde_json::to_string(&overview).unwrap();
    assert!(!json.contains("native"));
    assert!(!json.contains("file_path"));
    assert!(!json.contains("scores"));
}

#[test]
fn agent_kind_serializes_lowercase() {
    let conn = setup();
    seed(&conn);
    let resp = search_sessions(&conn, SearchParams::default()).unwrap();
    let json = serde_json::to_string(&resp).unwrap();
    assert!(json.contains("\"agent\":\"claude\""));
    assert!(json.contains("\"agent\":\"codex\""));
}

#[test]
fn paged_messages_skip_internal_marker_text() {
    let conn = setup();
    insert_session(&conn, "guarded", "claude", 1, None, None, None);
    insert_msg(&conn, "guarded", 0, "user", "real question");
    // A non-visible internal marker that should never reach MCP output, even if
    // it somehow lands in the messages table.
    insert_msg(
        &conn,
        "guarded",
        1,
        "user",
        "<system-reminder>internal</system-reminder>",
    );
    insert_msg(&conn, "guarded", 2, "assistant", "real answer");

    let page = get_session_messages(
        &conn,
        MessagesParams {
            id: "guarded".to_string(),
            limit: Some(30),
            order: MessageOrder::Asc,
            after_message_index: None,
            before_message_index: None,
        },
    )
    .unwrap()
    .unwrap();

    let indices: Vec<u32> = page.messages.iter().map(|m| m.message_index).collect();
    assert_eq!(indices, vec![0, 2]);
    assert!(page
        .messages
        .iter()
        .all(|m| !m.text.contains("system-reminder")));
}
