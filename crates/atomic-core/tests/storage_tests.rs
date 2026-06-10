//! Parameterized storage tests that run against both SQLite and Postgres backends.
//!
//! SQLite tests always run. Postgres tests require:
//!   - The `postgres` feature enabled
//!   - `ATOMIC_TEST_DATABASE_URL` env var set to a Postgres connection string
//!
//! Usage:
//!   cargo test -p atomic-core --test storage_tests                         # SQLite only
//!   cargo test -p atomic-core --test storage_tests --features postgres     # Both
//!
//! Note: Postgres tests must run serially (they share one DB):
//!   cargo test -p atomic-core --test storage_tests --features postgres -- postgres_tests --test-threads=1

use atomic_core::db::Database;
use atomic_core::models::*;
use atomic_core::storage::traits::*;
use atomic_core::storage::SqliteStorage;
use atomic_core::{AtomicCoreError, CreateAtomRequest, ListAtomsParams, UpdateAtomRequest};
use std::sync::Arc;
use tempfile::TempDir;

// ==================== Test Helpers ====================

async fn sqlite_storage() -> (SqliteStorage, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = Database::open_or_create(dir.path().join("test.db")).unwrap();
    let storage = SqliteStorage::new(Arc::new(db));
    (storage, dir)
}

#[cfg(feature = "postgres")]
async fn postgres_storage() -> Option<atomic_core::storage::PostgresStorage> {
    let url = match std::env::var("ATOMIC_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return None,
    };
    let storage = atomic_core::storage::PostgresStorage::connect(&url, "test")
        .await
        .unwrap();
    storage.initialize().await.unwrap();

    // Truncate data tables for a clean test (preserve schema)
    sqlx::raw_sql(
        "TRUNCATE atoms, tags, atom_tags, atom_chunks, atom_positions, atom_pipeline_jobs, \
         semantic_edges, atom_clusters, tag_embeddings, \
         wiki_articles, wiki_citations, wiki_links, wiki_article_versions, atom_links, \
         conversations, conversation_tags, chat_messages, chat_tool_calls, chat_citations, \
         feeds, feed_tags, feed_items, settings, task_runs, \
         briefing_citations, briefings, oauth_codes, oauth_clients, api_tokens \
         CASCADE",
    )
    .execute(storage.pool())
    .await
    .ok();

    Some(storage)
}

// ==================== AtomStore Tests ====================

async fn test_create_and_get_atom(storage: &dyn AtomStore) {
    let request = CreateAtomRequest {
        content: "# Test Atom\n\nThis is a test.".to_string(),
        source_url: None,
        published_at: None,
        tag_ids: vec![],
        ..Default::default()
    };

    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let created = storage.insert_atom(&id, &request, &now).await.unwrap();

    assert_eq!(created.atom.id, id);
    assert_eq!(created.atom.content, "# Test Atom\n\nThis is a test.");
    assert_eq!(created.atom.embedding_status, "pending");

    // Retrieve it
    let fetched = storage.get_atom(&id).await.unwrap();
    assert!(fetched.is_some());
    let fetched = fetched.unwrap();
    assert_eq!(fetched.atom.id, id);
    assert_eq!(fetched.atom.content, created.atom.content);
}

async fn test_get_atom_not_found(storage: &dyn AtomStore) {
    let result = storage.get_atom("nonexistent-id").await.unwrap();
    assert!(result.is_none());
}

async fn test_delete_atom(storage: &dyn AtomStore) {
    let request = CreateAtomRequest {
        content: "To be deleted".to_string(),
        source_url: None,
        published_at: None,
        tag_ids: vec![],
        ..Default::default()
    };
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    storage.insert_atom(&id, &request, &now).await.unwrap();

    storage.delete_atom(&id).await.unwrap();
    let result = storage.get_atom(&id).await.unwrap();
    assert!(result.is_none());
}

async fn test_update_atom(storage: &dyn AtomStore) {
    let request = CreateAtomRequest {
        content: "Original content".to_string(),
        source_url: None,
        published_at: None,
        tag_ids: vec![],
        ..Default::default()
    };
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    storage.insert_atom(&id, &request, &now).await.unwrap();

    let update = UpdateAtomRequest {
        content: "Updated content".to_string(),
        source_url: None,
        published_at: None,
        tag_ids: None,
    };
    let updated = storage.update_atom(&id, &update, &now).await.unwrap();
    assert_eq!(updated.atom.content, "Updated content");

    let fetched = storage.get_atom(&id).await.unwrap().unwrap();
    assert_eq!(fetched.atom.content, "Updated content");
}

async fn test_update_atom_if_unchanged_rejects_stale_write(storage: &dyn AtomStore) {
    let id = uuid::Uuid::new_v4().to_string();
    let created_at = "2024-01-01T00:00:00Z";
    storage
        .insert_atom(
            &id,
            &CreateAtomRequest {
                content: "Original content".to_string(),
                ..Default::default()
            },
            created_at,
        )
        .await
        .unwrap();

    let update = UpdateAtomRequest {
        content: "First edit".to_string(),
        source_url: None,
        published_at: None,
        tag_ids: None,
    };
    storage
        .update_atom_if_unchanged(&id, &update, "2024-01-02T00:00:00Z", created_at)
        .await
        .unwrap();

    let stale_update = UpdateAtomRequest {
        content: "Stale edit".to_string(),
        source_url: None,
        published_at: None,
        tag_ids: None,
    };
    let error = storage
        .update_atom_if_unchanged(&id, &stale_update, "2024-01-03T00:00:00Z", created_at)
        .await
        .unwrap_err();

    assert!(matches!(error, AtomicCoreError::Conflict(_)));
    let fetched = storage.get_atom(&id).await.unwrap().unwrap();
    assert_eq!(fetched.atom.content, "First edit");
}

async fn test_atom_links_materialized(storage: &dyn AtomStore) {
    let target_id = uuid::Uuid::new_v4().to_string();
    let source_id = uuid::Uuid::new_v4().to_string();
    let missing_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    storage
        .insert_atom(
            &target_id,
            &CreateAtomRequest {
                content: "# Target Atom\n\nBody".to_string(),
                ..Default::default()
            },
            &now,
        )
        .await
        .unwrap();
    storage
        .insert_atom(
            &source_id,
            &CreateAtomRequest {
                content: format!(
                    "Links: [[{}|Target label]], [[{}]], [[future-slug]]",
                    target_id, missing_id
                ),
                ..Default::default()
            },
            &now,
        )
        .await
        .unwrap();

    let links = storage.get_atom_links(&source_id).await.unwrap();
    assert_eq!(links.len(), 3);
    assert_eq!(links[0].target_atom_id.as_deref(), Some(target_id.as_str()));
    assert_eq!(links[0].target_title.as_deref(), Some("Target Atom"));
    assert_eq!(links[0].label.as_deref(), Some("Target label"));
    assert_eq!(links[0].target_kind, "atom_id");
    assert_eq!(links[0].status, "resolved");
    assert_eq!(links[1].raw_target, missing_id);
    assert_eq!(links[1].status, "missing");
    assert_eq!(links[2].raw_target, "future-slug");
    assert_eq!(links[2].target_kind, "text");
    assert_eq!(links[2].status, "unresolved");
}

async fn test_atom_links_replaced_on_update(storage: &dyn AtomStore) {
    let target_id = uuid::Uuid::new_v4().to_string();
    let source_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    storage
        .insert_atom(
            &target_id,
            &CreateAtomRequest {
                content: "# Target".to_string(),
                ..Default::default()
            },
            &now,
        )
        .await
        .unwrap();
    storage
        .insert_atom(
            &source_id,
            &CreateAtomRequest {
                content: format!("[[{}]]", target_id),
                ..Default::default()
            },
            &now,
        )
        .await
        .unwrap();

    assert_eq!(storage.get_atom_links(&source_id).await.unwrap().len(), 1);

    storage
        .update_atom_content_only(
            &source_id,
            &UpdateAtomRequest {
                content: "No links now".to_string(),
                source_url: None,
                published_at: None,
                tag_ids: None,
            },
            &chrono::Utc::now().to_rfc3339(),
        )
        .await
        .unwrap();

    assert!(storage.get_atom_links(&source_id).await.unwrap().is_empty());
}

async fn test_atom_links_mark_target_missing_on_delete(storage: &dyn AtomStore) {
    let target_id = uuid::Uuid::new_v4().to_string();
    let source_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    storage
        .insert_atom(
            &target_id,
            &CreateAtomRequest {
                content: "# Target".to_string(),
                ..Default::default()
            },
            &now,
        )
        .await
        .unwrap();
    storage
        .insert_atom(
            &source_id,
            &CreateAtomRequest {
                content: format!("[[{}|Target]]", target_id),
                ..Default::default()
            },
            &now,
        )
        .await
        .unwrap();

    storage.delete_atom(&target_id).await.unwrap();

    let links = storage.get_atom_links(&source_id).await.unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].raw_target, target_id);
    assert_eq!(links[0].target_atom_id, None);
    assert_eq!(links[0].status, "missing");
}

async fn test_atom_link_suggestions_recent_and_title_ranked(storage: &dyn AtomStore) {
    let older = "2024-01-01T00:00:00Z";
    let newer = "2024-01-02T00:00:00Z";
    let newest = "2024-01-03T00:00:00Z";

    let exact_id = uuid::Uuid::new_v4().to_string();
    let prefix_id = uuid::Uuid::new_v4().to_string();
    let contains_id = uuid::Uuid::new_v4().to_string();
    let body_only_id = uuid::Uuid::new_v4().to_string();

    storage
        .insert_atom(
            &prefix_id,
            &CreateAtomRequest {
                content: "# Project Atlas Notes\n\nBody".to_string(),
                ..Default::default()
            },
            older,
        )
        .await
        .unwrap();
    storage
        .insert_atom(
            &contains_id,
            &CreateAtomRequest {
                content: "# Notes for Project Atlas\n\nBody".to_string(),
                ..Default::default()
            },
            newer,
        )
        .await
        .unwrap();
    storage
        .insert_atom(
            &exact_id,
            &CreateAtomRequest {
                content: "# Project Atlas\n\nBody".to_string(),
                ..Default::default()
            },
            older,
        )
        .await
        .unwrap();
    storage
        .insert_atom(
            &body_only_id,
            &CreateAtomRequest {
                content: "# Unrelated\n\nProject Atlas only appears in body.".to_string(),
                ..Default::default()
            },
            newest,
        )
        .await
        .unwrap();

    let recent = storage.suggest_atom_links("", 2).await.unwrap();
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0].id, body_only_id);
    assert_eq!(recent[1].id, contains_id);

    let matches = storage
        .suggest_atom_links("project atlas", 10)
        .await
        .unwrap();
    let ids: Vec<&str> = matches.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, vec![exact_id, prefix_id, contains_id]);
    assert!(!ids.contains(&body_only_id.as_str()));
}

async fn test_get_all_atoms(storage: &dyn AtomStore) {
    let now = chrono::Utc::now().to_rfc3339();
    for i in 0..3 {
        let request = CreateAtomRequest {
            content: format!("Atom {}", i),
            source_url: None,
            published_at: None,
            tag_ids: vec![],
            ..Default::default()
        };
        let id = uuid::Uuid::new_v4().to_string();
        storage.insert_atom(&id, &request, &now).await.unwrap();
    }

    let all = storage.get_all_atoms().await.unwrap();
    assert!(all.len() >= 3);
}

async fn test_list_atoms_pagination(storage: &dyn AtomStore) {
    let now = chrono::Utc::now().to_rfc3339();
    for i in 0..5 {
        let request = CreateAtomRequest {
            content: format!("Paginated atom {}", i),
            source_url: None,
            published_at: None,
            tag_ids: vec![],
            ..Default::default()
        };
        let id = uuid::Uuid::new_v4().to_string();
        storage.insert_atom(&id, &request, &now).await.unwrap();
    }

    let params = ListAtomsParams {
        tag_id: None,
        limit: 2,
        offset: 0,
        cursor: None,
        cursor_id: None,
        source_filter: SourceFilter::All,
        source_value: None,
        sort_by: SortField::Updated,
        sort_order: SortOrder::Desc,
    };

    let page = storage
        .list_atoms(&params, &atomic_core::models::KindFilter::All)
        .await
        .unwrap();
    assert_eq!(page.atoms.len(), 2);
    assert!(page.total_count >= 5);
}

// ==================== TagStore Tests ====================

async fn test_create_and_get_tags(storage: &dyn TagStore) {
    let tag = storage.create_tag("Test Tag", None).await.unwrap();
    assert_eq!(tag.name, "Test Tag");
    assert!(tag.parent_id.is_none());

    let child = storage
        .create_tag("Child Tag", Some(&tag.id))
        .await
        .unwrap();
    assert_eq!(child.parent_id.as_deref(), Some(tag.id.as_str()));

    // get_all_tags returns a tree — flatten to count
    let all_tags = storage.get_all_tags().await.unwrap();
    fn count_tree(tags: &[TagWithCount]) -> usize {
        tags.iter().map(|t| 1 + count_tree(&t.children)).sum()
    }
    assert!(count_tree(&all_tags) >= 2);
}

async fn test_update_tag(storage: &dyn TagStore) {
    let tag = storage.create_tag("Old Name", None).await.unwrap();
    let updated = storage.update_tag(&tag.id, "New Name", None).await.unwrap();
    assert_eq!(updated.name, "New Name");
}

async fn test_delete_tag(storage: &dyn TagStore) {
    let tag = storage.create_tag("Doomed", None).await.unwrap();
    storage.delete_tag(&tag.id, false).await.unwrap();

    // Tag should be gone from get_all_tags
    let tags = storage.get_all_tags().await.unwrap();
    assert!(!tags.iter().any(|t| t.tag.id == tag.id));
}

async fn test_get_tag(storage: &dyn TagStore) {
    let tag = storage.create_tag("Lookup Tag", None).await.unwrap();
    let fetched = storage.get_tag(&tag.id).await.unwrap();
    assert_eq!(
        fetched.as_ref().map(|t| t.name.as_str()),
        Some("Lookup Tag")
    );
    assert_eq!(fetched.unwrap().id, tag.id);

    let missing = storage.get_tag("nonexistent-tag-id").await.unwrap();
    assert!(missing.is_none());
}

// ==================== TaskRunStore Tests ====================

/// Build a `TaskRun` row with caller-controlled state/timing fields —
/// `insert_task_run`'s contract is that the caller owns every column.
fn task_run_row(
    task_id: &str,
    subject_id: &str,
    state: TaskRunState,
    next_attempt_at: &str,
    lease_until: Option<&str>,
) -> TaskRun {
    let now = chrono::Utc::now().to_rfc3339();
    TaskRun {
        id: uuid::Uuid::new_v4().to_string(),
        task_id: task_id.to_string(),
        subject_id: Some(subject_id.to_string()),
        state,
        trigger: TaskRunTrigger::Schedule,
        attempts: 1,
        max_attempts: 3,
        lease_until: lease_until.map(String::from),
        next_attempt_at: next_attempt_at.to_string(),
        scope: None,
        result_id: None,
        last_error: None,
        started_at: None,
        finished_at: None,
        created_at: now.clone(),
        updated_at: now,
    }
}

/// The sweep query behind event-triggered retries (wiki regen): every
/// pending row past its `next_attempt_at` plus every running row whose
/// lease expired, across all subjects, earliest `next_attempt_at` first —
/// and nothing else.
async fn test_list_runnable_task_runs(storage: &dyn TaskRunStore) {
    // Unique task id per invocation so reruns against a shared Postgres
    // database can't see a previous run's rows.
    let task_id = format!("sweep_test::{}", uuid::Uuid::new_v4());
    let now = chrono::Utc::now();
    let past = (now - chrono::Duration::minutes(5)).to_rfc3339();
    let earlier_past = (now - chrono::Duration::minutes(10)).to_rfc3339();
    let future = (now + chrono::Duration::minutes(5)).to_rfc3339();

    let due = task_run_row(&task_id, "subject-due", TaskRunState::Pending, &past, None);
    let crashed = task_run_row(
        &task_id,
        "subject-crashed",
        TaskRunState::Running,
        &earlier_past,
        Some(&past), // lease expired — crash-recovery candidate
    );
    let backed_off = task_run_row(
        &task_id,
        "subject-backoff",
        TaskRunState::Pending,
        &future,
        None,
    );
    let in_flight = task_run_row(
        &task_id,
        "subject-live",
        TaskRunState::Running,
        &past,
        Some(&future), // live lease — owned by another worker
    );
    let settled = task_run_row(
        &task_id,
        "subject-done",
        TaskRunState::Succeeded,
        &past,
        None,
    );
    for row in [&due, &crashed, &backed_off, &in_flight, &settled] {
        storage.insert_task_run(row).await.unwrap();
    }

    let runnable = storage
        .list_runnable_task_runs(&task_id, &now.to_rfc3339())
        .await
        .unwrap();
    let ids: Vec<&str> = runnable.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![crashed.id.as_str(), due.id.as_str()],
        "exactly the due-pending and expired-lease rows, earliest next_attempt_at first"
    );

    // Other task ids never bleed into the sweep.
    let other = storage
        .list_runnable_task_runs("some_other_task", &now.to_rfc3339())
        .await
        .unwrap();
    assert!(other.iter().all(|r| r.task_id != task_id));
}

// ==================== ChatStore Tests ====================

async fn test_create_conversation(storage: &dyn ChatStore) {
    let conv = storage.create_conversation(&[], None).await.unwrap();
    assert!(!conv.conversation.id.is_empty());

    let fetched = storage
        .get_conversation(&conv.conversation.id)
        .await
        .unwrap();
    assert!(fetched.is_some());
}

async fn test_save_and_get_messages(storage: &dyn ChatStore) {
    let conv = storage
        .create_conversation(&[], Some("Test Chat"))
        .await
        .unwrap();

    let msg = storage
        .save_message(&conv.conversation.id, "user", "Hello!")
        .await
        .unwrap();
    assert_eq!(msg.role, "user");
    assert_eq!(msg.content, "Hello!");

    let full = storage
        .get_conversation(&conv.conversation.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(full.messages.len(), 1);
}

async fn test_delete_conversation(storage: &dyn ChatStore) {
    let conv = storage.create_conversation(&[], None).await.unwrap();
    storage
        .delete_conversation(&conv.conversation.id)
        .await
        .unwrap();

    let fetched = storage
        .get_conversation(&conv.conversation.id)
        .await
        .unwrap();
    assert!(fetched.is_none());
}

// ==================== WikiStore Tests ====================

async fn test_save_and_get_wiki(tag_store: &dyn TagStore, wiki_store: &dyn WikiStore) {
    // Wiki articles reference tags via FK, so create a tag first
    let tag = tag_store.create_tag("Wiki Test Tag", None).await.unwrap();
    let article = wiki_store
        .save_wiki(&tag.id, "# Wiki Article\n\nContent here.", &[], 5)
        .await
        .unwrap();
    assert_eq!(article.article.tag_id, tag.id);

    let fetched = wiki_store.get_wiki(&tag.id).await.unwrap();
    assert!(fetched.is_some());
    assert_eq!(
        fetched.unwrap().article.content,
        "# Wiki Article\n\nContent here."
    );
}

async fn test_delete_wiki(tag_store: &dyn TagStore, wiki_store: &dyn WikiStore) {
    let tag = tag_store.create_tag("Wiki Delete Tag", None).await.unwrap();
    wiki_store.save_wiki(&tag.id, "temp", &[], 1).await.unwrap();
    wiki_store.delete_wiki(&tag.id).await.unwrap();

    let fetched = wiki_store.get_wiki(&tag.id).await.unwrap();
    assert!(fetched.is_none());
}

async fn test_wiki_update_chunks_pending_atom_errors(
    atom_store: &dyn AtomStore,
    tag_store: &dyn TagStore,
    wiki_store: &dyn WikiStore,
) {
    let tag = tag_store
        .create_tag("Wiki Pending Tag", None)
        .await
        .unwrap();
    let request = CreateAtomRequest {
        content: "This atom has not been chunked yet.".to_string(),
        source_url: None,
        published_at: None,
        tag_ids: vec![tag.id.clone()],
        ..Default::default()
    };
    let atom_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    atom_store
        .insert_atom(&atom_id, &request, &now)
        .await
        .unwrap();

    let result = wiki_store
        .get_wiki_update_chunks(&tag.id, "1970-01-01T00:00:00Z", 1024)
        .await;

    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("not ready for wiki update"));
}

// ==================== ChunkStore Tests ====================

// Embedding status tests need both AtomStore and ChunkStore together,
// which is tested through AtomicCore integration tests.

// ==================== SQLite Test Runners ====================

#[tokio::test]
async fn sqlite_create_and_get_atom() {
    let (s, _dir) = sqlite_storage().await;
    test_create_and_get_atom(&s).await;
}

#[tokio::test]
async fn sqlite_get_atom_not_found() {
    let (s, _dir) = sqlite_storage().await;
    test_get_atom_not_found(&s).await;
}

#[tokio::test]
async fn sqlite_delete_atom() {
    let (s, _dir) = sqlite_storage().await;
    test_delete_atom(&s).await;
}

#[tokio::test]
async fn sqlite_update_atom() {
    let (s, _dir) = sqlite_storage().await;
    test_update_atom(&s).await;
}

#[tokio::test]
async fn sqlite_update_atom_if_unchanged_rejects_stale_write() {
    let (s, _dir) = sqlite_storage().await;
    test_update_atom_if_unchanged_rejects_stale_write(&s).await;
}

#[tokio::test]
async fn sqlite_atom_links_materialized() {
    let (s, _dir) = sqlite_storage().await;
    test_atom_links_materialized(&s).await;
}

#[tokio::test]
async fn sqlite_atom_links_replaced_on_update() {
    let (s, _dir) = sqlite_storage().await;
    test_atom_links_replaced_on_update(&s).await;
}

#[tokio::test]
async fn sqlite_atom_links_mark_target_missing_on_delete() {
    let (s, _dir) = sqlite_storage().await;
    test_atom_links_mark_target_missing_on_delete(&s).await;
}

#[tokio::test]
async fn sqlite_atom_link_suggestions_recent_and_title_ranked() {
    let (s, _dir) = sqlite_storage().await;
    test_atom_link_suggestions_recent_and_title_ranked(&s).await;
}

#[tokio::test]
async fn sqlite_get_all_atoms() {
    let (s, _dir) = sqlite_storage().await;
    test_get_all_atoms(&s).await;
}

#[tokio::test]
async fn sqlite_list_atoms_pagination() {
    let (s, _dir) = sqlite_storage().await;
    test_list_atoms_pagination(&s).await;
}

#[tokio::test]
async fn sqlite_create_and_get_tags() {
    let (s, _dir) = sqlite_storage().await;
    test_create_and_get_tags(&s).await;
}

#[tokio::test]
async fn sqlite_update_tag() {
    let (s, _dir) = sqlite_storage().await;
    test_update_tag(&s).await;
}

#[tokio::test]
async fn sqlite_delete_tag() {
    let (s, _dir) = sqlite_storage().await;
    test_delete_tag(&s).await;
}

#[tokio::test]
async fn sqlite_get_tag() {
    let (s, _dir) = sqlite_storage().await;
    test_get_tag(&s).await;
}

#[tokio::test]
async fn sqlite_list_runnable_task_runs() {
    let (s, _dir) = sqlite_storage().await;
    test_list_runnable_task_runs(&s).await;
}

#[tokio::test]
async fn sqlite_create_conversation() {
    let (s, _dir) = sqlite_storage().await;
    test_create_conversation(&s).await;
}

#[tokio::test]
async fn sqlite_save_and_get_messages() {
    let (s, _dir) = sqlite_storage().await;
    test_save_and_get_messages(&s).await;
}

#[tokio::test]
async fn sqlite_delete_conversation() {
    let (s, _dir) = sqlite_storage().await;
    test_delete_conversation(&s).await;
}

#[tokio::test]
async fn sqlite_save_and_get_wiki() {
    let (s, _dir) = sqlite_storage().await;
    test_save_and_get_wiki(&s, &s).await;
}

#[tokio::test]
async fn sqlite_delete_wiki() {
    let (s, _dir) = sqlite_storage().await;
    test_delete_wiki(&s, &s).await;
}

#[tokio::test]
async fn sqlite_wiki_update_chunks_pending_atom_errors() {
    let (s, _dir) = sqlite_storage().await;
    test_wiki_update_chunks_pending_atom_errors(&s, &s, &s).await;
}

// ==================== Postgres Test Runners ====================

#[cfg(feature = "postgres")]
mod postgres_tests {
    use super::*;

    /// Helper macro: skip test if ATOMIC_TEST_DATABASE_URL is not set
    macro_rules! pg_test {
        ($name:ident, $body:expr) => {
            #[tokio::test]
            async fn $name() {
                let Some(ref s) = postgres_storage().await else {
                    eprintln!(
                        "Skipping {} (ATOMIC_TEST_DATABASE_URL not set)",
                        stringify!($name)
                    );
                    return;
                };
                $body(s).await;
            }
        };
    }

    pg_test!(pg_create_and_get_atom, test_create_and_get_atom);
    pg_test!(pg_get_atom_not_found, test_get_atom_not_found);
    pg_test!(pg_delete_atom, test_delete_atom);
    pg_test!(pg_update_atom, test_update_atom);
    pg_test!(
        pg_update_atom_if_unchanged_rejects_stale_write,
        test_update_atom_if_unchanged_rejects_stale_write
    );
    pg_test!(pg_atom_links_materialized, test_atom_links_materialized);
    pg_test!(
        pg_atom_links_replaced_on_update,
        test_atom_links_replaced_on_update
    );
    pg_test!(
        pg_atom_links_mark_target_missing_on_delete,
        test_atom_links_mark_target_missing_on_delete
    );
    pg_test!(
        pg_atom_link_suggestions_recent_and_title_ranked,
        test_atom_link_suggestions_recent_and_title_ranked
    );
    pg_test!(pg_get_all_atoms, test_get_all_atoms);
    pg_test!(pg_list_atoms_pagination, test_list_atoms_pagination);
    pg_test!(pg_create_and_get_tags, test_create_and_get_tags);
    pg_test!(pg_update_tag, test_update_tag);
    pg_test!(pg_delete_tag, test_delete_tag);
    pg_test!(pg_get_tag, test_get_tag);
    pg_test!(pg_list_runnable_task_runs, test_list_runnable_task_runs);
    pg_test!(pg_create_conversation, test_create_conversation);
    pg_test!(pg_save_and_get_messages, test_save_and_get_messages);
    pg_test!(pg_delete_conversation, test_delete_conversation);

    /// Re-initializing an already-migrated database must be a no-op. A
    /// version-read failure (or type-mismatched decode) that silently
    /// defaults to 0 re-runs every migration from 1, which appends duplicate
    /// schema_version rows on every open — exactly what this guards against.
    #[tokio::test]
    async fn pg_initialize_reopen_runs_no_migrations() {
        let Some(ref s) = postgres_storage().await else {
            eprintln!("Skipping (ATOMIC_TEST_DATABASE_URL not set)");
            return;
        };
        let url = std::env::var("ATOMIC_TEST_DATABASE_URL").unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        let count = |pool: sqlx::PgPool| async move {
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM schema_version")
                .fetch_one(&pool)
                .await
                .unwrap()
        };
        let before = count(pool.clone()).await;
        s.initialize().await.unwrap();
        let after = count(pool).await;
        assert_eq!(
            before, after,
            "initialize() on a migrated database re-ran migrations \
             ({before} schema_version rows -> {after})"
        );
    }

    #[tokio::test]
    async fn pg_save_and_get_wiki() {
        let Some(ref s) = postgres_storage().await else {
            eprintln!("Skipping (ATOMIC_TEST_DATABASE_URL not set)");
            return;
        };
        test_save_and_get_wiki(s, s).await;
    }

    #[tokio::test]
    async fn pg_delete_wiki() {
        let Some(ref s) = postgres_storage().await else {
            eprintln!("Skipping (ATOMIC_TEST_DATABASE_URL not set)");
            return;
        };
        test_delete_wiki(s, s).await;
    }

    #[tokio::test]
    async fn pg_wiki_update_chunks_pending_atom_errors() {
        let Some(ref s) = postgres_storage().await else {
            eprintln!("Skipping (ATOMIC_TEST_DATABASE_URL not set)");
            return;
        };
        test_wiki_update_chunks_pending_atom_errors(s, s, s).await;
    }
}
