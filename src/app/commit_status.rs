//! Prepare every commit's review population together, independently of which
//! commit the reviewer visits. Rendering only consults these fingerprints and
//! the live session; no VCS or forge calls happen while painting rows.

use super::*;
use std::sync::mpsc::{Receiver, channel};

type FileFingerprints = Vec<(PathBuf, u64, Vec<String>)>;

#[derive(PartialEq, Eq)]
struct PreparationKey {
    source: DiffSource,
    commits: Vec<String>,
    full_diff: Vec<(PathBuf, u64)>,
}

enum PreparedDiffs {
    Local(Vec<Option<FileFingerprints>>),
    Remote(Vec<Option<Vec<crate::model::FilePatch>>>),
}

pub(super) struct CommitReviewPreparation {
    key: PreparationKey,
    rx: Option<Receiver<PreparedDiffs>>,
    files: Vec<Option<FileFingerprints>>,
}

fn fingerprints(files: Vec<DiffFile>) -> FileFingerprints {
    files
        .into_iter()
        .filter(|file| !file.is_commit_message)
        .map(|file| {
            (
                file.display_path().clone(),
                file.content_hash,
                file.hunk_review_keys(),
            )
        })
        .collect()
}

impl App {
    fn commit_review_preparation_key(&self) -> PreparationKey {
        PreparationKey {
            source: self.diff_source.clone(),
            commits: self
                .review_commits
                .iter()
                .map(|commit| commit.id.clone())
                .collect(),
            full_diff: self
                .range_diff_files
                .as_deref()
                .unwrap_or_default()
                .iter()
                .filter(|file| !file.is_commit_message)
                .map(|file| (file.display_path().clone(), file.content_hash))
                .collect(),
        }
    }

    /// Start one eager batch when the review changes, then install its results
    /// together. Cycling the selection is deliberately not part of the key.
    pub fn poll_commit_review_preparation(&mut self) -> bool {
        if !self.has_review_commits() {
            return self.commit_review_preparation.take().is_some();
        }
        if self.input_mode == InputMode::CommitSelect {
            return false;
        }
        let key = self.commit_review_preparation_key();
        if self
            .commit_review_preparation
            .as_ref()
            .is_none_or(|state| state.key != key)
        {
            let rx = self.spawn_commit_review_preparation();
            self.commit_review_preparation = Some(CommitReviewPreparation {
                files: vec![None; key.commits.len()],
                key,
                rx: Some(rx),
            });
            return true;
        }
        let state = self.commit_review_preparation.as_mut().unwrap();
        let Some(result) = state.rx.as_ref().and_then(|rx| rx.try_recv().ok()) else {
            return false;
        };
        state.rx = None;
        state.files = match result {
            PreparedDiffs::Local(files) => files,
            PreparedDiffs::Remote(patches) => {
                let local_checkout = self
                    .forge_backend
                    .as_deref()
                    .and_then(|backend| backend.local_checkout_path());
                patches
                    .into_iter()
                    .map(|patches| {
                        let patches = patches?;
                        let patches = match local_checkout.as_deref() {
                            Some(root) => crate::tuicrignore::filter_file_patches(root, patches),
                            None => patches,
                        };
                        match crate::vcs::diff_parser::parse_file_patches(
                            patches,
                            self.theme.syntax_highlighter(),
                        ) {
                            Ok(files) => Some(fingerprints(files)),
                            Err(TuicrError::NoChanges) => Some(Vec::new()),
                            Err(_) => None,
                        }
                    })
                    .collect()
            }
        };
        true
    }

    fn spawn_commit_review_preparation(&self) -> Receiver<PreparedDiffs> {
        let (tx, rx) = channel();
        let commits = self.review_commits.clone();
        if let DiffSource::PullRequest(pr) = &self.diff_source {
            let details = self.pr_info.as_ref().map(|info| {
                let mut details = info.details.clone();
                details.base_sha = pr.base_sha.clone();
                details.head_sha = pr.key.head_sha.clone();
                details
            });
            let local_checkout = self
                .forge_backend
                .as_deref()
                .and_then(|backend| backend.local_checkout_path());
            std::thread::spawn(move || {
                let Some(details) = details else { return };
                let backend =
                    create_forge_backend(&details.repository, local_checkout, false, false);
                let patches = commits
                    .iter()
                    .enumerate()
                    .map(|(index, commit)| {
                        let parent = commits
                            .get(index + 1)
                            .map(|commit| commit.id.as_str())
                            .unwrap_or(&details.base_sha);
                        backend
                            .get_pull_request_commit_range_diff(&details, parent, &commit.id)
                            .ok()
                    })
                    .collect();
                let _ = tx.send(PreparedDiffs::Remote(patches));
            });
        } else {
            let options = self.vcs_open_options.clone();
            let root = self.vcs_info.root_path.clone();
            let path_filter = self.path_filter.clone();
            let highlighter = self.theme.syntax_highlighter_arc();
            std::thread::spawn(move || {
                let vcs = detect_vcs(options.git_backend_preference, options.diff_whitespace_mode)
                    .ok()
                    .filter(|vcs| vcs.info().root_path == root);
                let files = (0..commits.len())
                    .map(|index| {
                        let vcs = vcs.as_ref()?;
                        let source = Self::source_for_commit_subrange(&commits, index, index);
                        match Self::fetch_diff_files_for_source(
                            vcs.as_ref(),
                            &root,
                            &source,
                            &highlighter,
                            path_filter.as_deref(),
                        ) {
                            Ok(files) => Some(fingerprints(files)),
                            Err(TuicrError::NoChanges) => Some(Vec::new()),
                            Err(_) => None,
                        }
                    })
                    .collect();
                let _ = tx.send(PreparedDiffs::Local(files));
            });
        }
        rx
    }

    pub fn commit_review_status(&self, index: usize) -> CommitReviewStatus {
        let Some(commit) = self.review_commits.get(index) else {
            return CommitReviewStatus::Empty;
        };
        let files = self
            .commit_review_preparation
            .as_ref()
            .filter(|state| {
                state.key.source == self.diff_source
                    && state.key.commits.get(index) == Some(&commit.id)
            })
            .and_then(|state| state.files.get(index))
            .and_then(Option::as_deref);
        let applies = |comment: &Comment| {
            comment
                .commit_id
                .as_deref()
                .is_none_or(|id| id == commit.id)
        };
        let message_path = PathBuf::from(format!("Commit Message ({})", commit.short_id));
        let has_comments = self.session.review_comments.iter().any(applies)
            || self
                .session
                .session_notes
                .as_deref()
                .is_some_and(|notes| !notes.trim().is_empty())
            || self.session.files.iter().any(|(path, review)| {
                let in_patch = path == &message_path
                    || files.is_some_and(|files| files.iter().any(|(file, _, _)| file == path));
                review
                    .file_comments
                    .iter()
                    .chain(review.line_comments.values().flatten())
                    .any(|comment| {
                        applies(comment)
                            && (in_patch
                                || comment.commit_id.as_deref() == Some(commit.id.as_str()))
                    })
            });
        if has_comments {
            return CommitReviewStatus::NeedsAttention;
        }
        let Some(files) = files else {
            return CommitReviewStatus::Empty;
        };
        let mut activity = self
            .session
            .files
            .get(&message_path)
            .is_some_and(|review| review.reviewed || !review.reviewed_hunks.is_empty());
        let mut all_reviewed = !files.is_empty();
        for (path, hash, hunk_keys) in files {
            let review = self.session.files.get(path);
            let reviewed = review.is_some_and(|review| review.is_content_reviewed(*hash));
            all_reviewed &= reviewed;
            activity |= reviewed
                || review.is_some_and(|review| {
                    hunk_keys
                        .iter()
                        .any(|key| review.reviewed_hunks.contains(key))
                });
        }
        if all_reviewed {
            CommitReviewStatus::Reviewed
        } else if activity {
            CommitReviewStatus::NeedsAttention
        } else {
            CommitReviewStatus::Empty
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepares_all_commits_before_visiting_them_and_tracks_live_feedback() {
        let mut app = crate::app::tests::render_perf_tests::app_with(Vec::new());
        app.review_commits = ["new", "old"]
            .into_iter()
            .map(|id| CommitInfo {
                id: id.into(),
                short_id: id.into(),
                change_id_prefix_len: None,
                branch_name: None,
                summary: id.into(),
                body: None,
                author: "tester".into(),
                time: Utc::now(),
            })
            .collect();
        app.diff_source = DiffSource::CommitRange(vec!["old".into(), "new".into()]);
        app.commit_selection_range = Some((0, 0));
        let path = PathBuf::from("a.rs");
        app.session.add_file(path.clone(), FileStatus::Modified, 1);
        app.session.files.get_mut(&path).unwrap().set_reviewed(true);
        app.session.add_file(path.clone(), FileStatus::Modified, 2);
        let (tx, rx) = channel();
        app.commit_review_preparation = Some(CommitReviewPreparation {
            key: app.commit_review_preparation_key(),
            rx: Some(rx),
            files: vec![None; 2],
        });
        assert_eq!(app.commit_review_status(0), CommitReviewStatus::Empty);
        tx.send(PreparedDiffs::Local(vec![
            Some(vec![(path.clone(), 1, vec!["first".into()])]),
            Some(vec![(path.clone(), 2, vec!["second".into()])]),
        ]))
        .unwrap();
        assert!(app.poll_commit_review_preparation());
        assert!(
            app.commit_diff_cache.is_empty(),
            "neither commit needs to have been visited"
        );
        assert_eq!(app.commit_review_status(0), CommitReviewStatus::Reviewed);
        assert_eq!(app.commit_review_status(1), CommitReviewStatus::Empty);
        app.session
            .files
            .get_mut(&path)
            .unwrap()
            .reviewed_hunks
            .insert("second".into());
        assert_eq!(
            app.commit_review_status(1),
            CommitReviewStatus::NeedsAttention
        );
        let review = app.session.files.get_mut(&path).unwrap();
        review.reviewed_hunks.clear();
        review.reviewed_hunks.insert("first".into());
        assert_eq!(
            app.commit_review_status(1),
            CommitReviewStatus::Empty,
            "another commit's file or hunk approval must not count as activity"
        );
        let mut comment = Comment::new("Feedback only".into(), CommentType::from_id("note"), None);
        comment.commit_id = Some("old".into());
        app.session
            .files
            .get_mut(&path)
            .unwrap()
            .file_comments
            .push(comment);
        assert_eq!(
            app.commit_review_status(1),
            CommitReviewStatus::NeedsAttention
        );
        app.session
            .files
            .get_mut(&path)
            .unwrap()
            .file_comments
            .clear();
        assert_eq!(app.commit_review_status(1), CommitReviewStatus::Empty);

        app.cycle_commit_next();
        assert!(
            !app.poll_commit_review_preparation(),
            "cycling must not re-prepare progress"
        );
        assert_eq!(app.commit_review_status(0), CommitReviewStatus::Reviewed);
        app.session.files.get_mut(&path).unwrap().set_reviewed(true);
        assert_eq!(app.commit_review_status(1), CommitReviewStatus::Reviewed);

        let mut comment = Comment::new("Fix this".into(), CommentType::from_id("note"), None);
        comment.commit_id = Some("new".into());
        app.session
            .files
            .get_mut(&path)
            .unwrap()
            .file_comments
            .push(comment);
        assert_eq!(
            app.commit_review_status(0),
            CommitReviewStatus::NeedsAttention
        );
        assert_eq!(app.commit_review_status(1), CommitReviewStatus::Reviewed);
        app.session.session_notes = Some("Review notes".into());
        assert_eq!(
            app.commit_review_status(1),
            CommitReviewStatus::NeedsAttention
        );
        app.session.session_notes = None;
        app.diff_source = DiffSource::CommitRange(vec!["different".into()]);
        assert_eq!(
            app.commit_review_status(1),
            CommitReviewStatus::Empty,
            "a different review must not use stale fingerprints"
        );
    }
}
