use crate::app::*;

const SOME_HASH: u64 = 0xabc;

fn test_session() -> ReviewSession {
    let mut session = ReviewSession::new(
        PathBuf::from("/repo"),
        "abc1234".to_string(),
        Some("main".to_string()),
        SessionDiffSource::WorkingTree,
    );
    session.add_file(
        PathBuf::from("src/main.rs"),
        FileStatus::Modified,
        SOME_HASH,
    );
    session
}

fn comment(id: &str, content: &str) -> Comment {
    let mut comment = Comment::new(content.to_string(), CommentType::from_id("note"), None);
    comment.id = id.to_string();
    comment
}

fn push_file_comment(session: &mut ReviewSession, id: &str, content: &str) {
    session
        .get_file_mut(&PathBuf::from("src/main.rs"))
        .unwrap()
        .file_comments
        .push(comment(id, content));
}

fn file_comment_ids(session: &ReviewSession) -> Vec<String> {
    session
        .files
        .get(&PathBuf::from("src/main.rs"))
        .unwrap()
        .file_comments
        .iter()
        .map(|comment| comment.id.clone())
        .collect()
}

#[test]
fn should_merge_external_comment_without_losing_local_comment() {
    let base = test_session();
    let mut current = base.clone();
    let mut latest = base.clone();

    push_file_comment(&mut current, "local", "from tui");
    push_file_comment(&mut latest, "external", "from cli");

    let changed = App::merge_external_session_changes(&mut current, &base, &latest);

    assert_eq!(changed, 1);
    assert_eq!(file_comment_ids(&current), vec!["local", "external"]);
}

#[test]
fn should_not_resurrect_locally_deleted_comment_when_disk_is_unchanged() {
    let mut base = test_session();
    push_file_comment(&mut base, "deleted", "old");
    let mut current = base.clone();
    current
        .get_file_mut(&PathBuf::from("src/main.rs"))
        .unwrap()
        .file_comments
        .clear();
    let latest = base.clone();

    let changed = App::merge_external_session_changes(&mut current, &base, &latest);

    assert_eq!(changed, 0);
    assert!(file_comment_ids(&current).is_empty());
}

#[test]
fn should_apply_external_edit_when_comment_is_unchanged_locally() {
    let mut base = test_session();
    push_file_comment(&mut base, "same", "old");
    let mut current = base.clone();
    let mut latest = base.clone();
    latest
        .get_file_mut(&PathBuf::from("src/main.rs"))
        .unwrap()
        .file_comments[0]
        .content = "new".to_string();

    let changed = App::merge_external_session_changes(&mut current, &base, &latest);

    assert_eq!(changed, 1);
    assert_eq!(
        current
            .files
            .get(&PathBuf::from("src/main.rs"))
            .unwrap()
            .file_comments[0]
            .content,
        "new"
    );
}

#[test]
fn should_merge_reviewed_hashes_without_transferring_approval_between_views() {
    let path = PathBuf::from("src/main.rs");
    let base = test_session();
    let mut current = base.clone();
    current.add_file(path.clone(), FileStatus::Modified, 123);
    current.files.get_mut(&path).unwrap().set_reviewed(true);
    let mut latest = base.clone();
    latest.files.get_mut(&path).unwrap().set_reviewed(true);

    App::merge_external_session_changes(&mut current, &base, &latest);
    let review = &current.files[&path];
    assert_eq!(review.content_hash, Some(123));
    assert!(review.reviewed);
    assert!(review.is_content_reviewed(SOME_HASH));

    // An external unmark only affects its own diff, not the locally reviewed one.
    let base = latest.clone();
    latest.files.get_mut(&path).unwrap().set_reviewed(false);
    App::merge_external_session_changes(&mut current, &base, &latest);
    assert!(current.files[&path].reviewed);
    assert!(!current.files[&path].is_content_reviewed(SOME_HASH));

    let saved = current.clone();
    current.files.get_mut(&path).unwrap().set_reviewed(false);
    App::merge_external_session_changes(&mut current, &saved, &saved);
    assert!(
        !current.has_reviewed_state(),
        "an unchanged disk must not resurrect a local unmark"
    );
}

#[test]
fn should_merge_legacy_reviewed_flags_without_overwriting_local_changes() {
    let path = PathBuf::from("src/main.rs");
    for (base_reviewed, current_reviewed, latest_reviewed, expected) in [
        (true, true, false, false),
        (false, false, true, true),
        (true, false, true, false),
        (false, true, false, true),
    ] {
        let mut base = test_session();
        let review = base.files.get_mut(&path).unwrap();
        review.content_hash = None;
        review.set_reviewed(base_reviewed);
        let mut current = base.clone();
        current
            .files
            .get_mut(&path)
            .unwrap()
            .set_reviewed(current_reviewed);
        let mut latest = base.clone();
        latest
            .files
            .get_mut(&path)
            .unwrap()
            .set_reviewed(latest_reviewed);

        let changed = App::merge_external_session_changes(&mut current, &base, &latest);

        assert_eq!(current.files[&path].reviewed, expected);
        assert_eq!(changed, usize::from(current_reviewed != expected));
        assert_eq!(current.files[&path].content_hash, None);
        // Saving again against the observed disk state must not undo the merge.
        assert_eq!(
            App::merge_external_session_changes(&mut current, &latest, &latest),
            0
        );
        assert_eq!(current.files[&path].reviewed, expected);
    }
}

#[test]
fn should_not_transfer_legacy_approval_to_a_registered_diff() {
    let path = PathBuf::from("src/main.rs");
    let mut base = test_session();
    base.files.get_mut(&path).unwrap().content_hash = None;
    let mut current = base.clone();
    current.add_file(path.clone(), FileStatus::Modified, SOME_HASH);
    let mut latest = base.clone();
    latest.files.get_mut(&path).unwrap().set_reviewed(true);

    assert_eq!(
        App::merge_external_session_changes(&mut current, &base, &latest),
        0
    );
    assert!(!current.files[&path].reviewed);
}

#[test]
fn should_not_transfer_registered_approval_to_a_legacy_flag() {
    let path = PathBuf::from("src/main.rs");
    for reviewed in [false, true] {
        let mut base = test_session();
        let review = base.files.get_mut(&path).unwrap();
        review.content_hash = None;
        review.set_reviewed(reviewed);
        let mut current = base.clone();
        let mut latest = base.clone();
        latest.add_file(path.clone(), FileStatus::Modified, SOME_HASH);
        latest.files.get_mut(&path).unwrap().set_reviewed(true);

        assert_eq!(
            App::merge_external_session_changes(&mut current, &base, &latest),
            1
        );
        assert_eq!(current.files[&path].reviewed, reviewed);
        assert!(current.files[&path].is_content_reviewed(SOME_HASH));
    }
}
