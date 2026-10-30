//! Plain-text, bottom-posted replies to a single local Git commit.
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write;
use std::path::Path;

use git2::{DiffOptions, Oid, Patch, Repository};
use unicode_width::UnicodeWidthStr;

use crate::app::CommentTypeDefinition;
use crate::error::{Result, TuicrError};
use crate::model::review::FileReview;
use crate::model::{Comment, DiffFile, LineRange, LineSide, ReviewSession};
use crate::vcs::CommitInfo;

const CONTEXT: usize = 10;
const COMMENT_WIDTH: usize = 72;

struct QuotedLine {
    text: String,
    old: Option<u32>,
    new: Option<u32>,
    hunk: usize,
}

/// Materialize the actual commit, not the worktree or the viewport's possibly
/// whitespace-filtered/expanded diff. Paths come from libgit2 metadata only.
pub fn generate_mail_content(
    repo_path: &Path,
    selected: &CommitInfo,
    session: &ReviewSession,
    comment_types: &[CommentTypeDefinition],
    diff_files: &[DiffFile],
) -> Result<String> {
    let repo = Repository::discover(repo_path)?;
    let commit = repo.find_commit(Oid::from_str(&selected.id)?)?;
    let tree = commit.tree()?;
    let parent = if commit.parent_count() == 0 {
        None
    } else {
        Some(commit.parent(0)?.tree()?)
    };
    let mut options = DiffOptions::new();
    // Full-file context lets comments made on expanded gaps retain their
    // real anchors too. Only commented files are materialized below, and
    // write_excerpt trims the actual email to ten lines around each comment.
    options.context_lines(i32::MAX as u32);
    let mut diff = repo.diff_tree_to_tree(parent.as_ref(), Some(&tree), Some(&mut options))?;
    diff.find_similar(None)?;

    let message_path = std::path::PathBuf::from(format!("Commit Message ({})", selected.short_id));
    let paths: HashSet<_> = diff
        .deltas()
        .filter_map(|d| {
            d.new_file()
                .path()
                .or(d.old_file().path())
                .map(Path::to_path_buf)
        })
        .collect();
    let visible = |c: &&Comment| c.commit_id.as_deref().is_none_or(|id| id == selected.id);
    let mut out = String::new();
    let mut emitted = HashSet::new();
    let message = commit.message().unwrap_or(&selected.summary);

    // The synthetic commit-message file numbers the subject as line 1,
    // including the blank separator before the body.
    let message_review = session.files.get(&message_path);
    let rows: Vec<_> = message
        .lines()
        .enumerate()
        .map(|(i, line)| QuotedLine {
            text: line.to_string(),
            old: None,
            new: Some(i as u32 + 1),
            hunk: 0,
        })
        .collect();
    // Keep the commit message even without comments: it identifies the reply.
    write_excerpt(
        &mut out,
        &rows,
        &[],
        message_review,
        &selected.id,
        comment_types,
        &mut emitted,
        true,
    );
    if let Some(review) = message_review {
        write_file_comments(&mut out, review, &selected.id, comment_types, &mut emitted);
    }
    if let Some(notes) = session.session_notes.as_deref().filter(|s| !s.is_empty()) {
        out.push('\n');
        write_wrapped(&mut out, notes);
        out.push('\n');
    }
    for comment in session.review_comments.iter().filter(visible) {
        write_comment(&mut out, comment, comment_types, &mut emitted);
    }
    quote(&mut out, "");
    quote(&mut out, "---");

    let mut seen = HashSet::from([message_path]);
    let mut skipped_files = false;
    for (index, delta) in diff.deltas().enumerate() {
        let Some(path) = delta.new_file().path().or(delta.old_file().path()) else {
            continue;
        };
        seen.insert(path.to_path_buf());
        let review = session.files.get(path);
        let has_comments = review.is_some_and(|r| {
            r.file_comments
                .iter()
                .chain(r.line_comments.values().flatten())
                .any(|c| visible(&c))
        });
        if !has_comments {
            skipped_files = true;
            continue;
        }
        if skipped_files {
            quote(&mut out, "[snip]");
            skipped_files = false;
        }
        if let Some(mut patch) = Patch::from_diff(&diff, index)? {
            let raw = patch.to_buf()?;
            // Only extracting display headers here; identity remains `delta`'s.
            for line in String::from_utf8_lossy(&raw)
                .lines()
                .take_while(|l| !l.starts_with("@@ "))
            {
                quote(&mut out, line);
            }
            let mut rows = Vec::new();
            let mut headers = Vec::new();
            for hunk_index in 0..patch.num_hunks() {
                let (hunk, count) = patch.hunk(hunk_index)?;
                headers.push(
                    String::from_utf8_lossy(hunk.header())
                        .trim_end_matches('\n')
                        .to_string(),
                );
                for line_index in 0..count {
                    let line = patch.line_in_hunk(hunk_index, line_index)?;
                    let text = match line.origin() {
                        ' ' | '+' | '-' => format!(
                            "{}{}",
                            line.origin(),
                            String::from_utf8_lossy(line.content()).trim_end_matches('\n')
                        ),
                        _ => "\\ No newline at end of file".to_string(),
                    };
                    rows.push(QuotedLine {
                        text,
                        old: line.old_lineno(),
                        new: line.new_lineno(),
                        hunk: hunk_index,
                    });
                }
            }
            if let Some(review) = review {
                write_file_comments(&mut out, review, &selected.id, comment_types, &mut emitted);
            }
            write_excerpt(
                &mut out,
                &rows,
                &headers,
                review,
                &selected.id,
                comment_types,
                &mut emitted,
                false,
            );
        } else {
            quote(&mut out, &format!("diff --git a/{0} b/{0}", path.display()));
            quote(&mut out, "Binary files differ");
            if let Some(review) = review {
                write_file_comments(&mut out, review, &selected.id, comment_types, &mut emitted);
            }
        }
    }
    if skipped_files {
        quote(&mut out, "[snip]");
    }

    // Expanded context, obsolete anchors and files outside this commit must
    // never silently lose comments or attach them to an unrelated line.
    let mut files: Vec<_> = session.files.iter().collect();
    files.sort_by_key(|(path, _)| *path);
    for (path, review) in files {
        if path.to_string_lossy().starts_with("Commit Message (") && !seen.contains(path) {
            continue;
        }
        let belongs_here = |c: &&Comment| {
            visible(c)
                && (seen.contains(path) || c.commit_id.as_deref() == Some(selected.id.as_str()))
        };
        for comment in review.file_comments.iter().filter(belongs_here) {
            if !emitted.contains(&comment.id) {
                let _ = writeln!(out, "\n{} (not in this patch):", path.display());
                write_comment(&mut out, comment, comment_types, &mut emitted);
            }
        }
        let mut lines: Vec<_> = review.line_comments.iter().collect();
        lines.sort_by_key(|(line, _)| *line);
        for (line, comments) in lines {
            for comment in comments.iter().filter(belongs_here) {
                if !emitted.contains(&comment.id) {
                    let side = if comment.side == Some(LineSide::Old) {
                        "old"
                    } else {
                        "new"
                    };
                    let range = comment.line_range.unwrap_or(LineRange::single(*line));
                    let _ = writeln!(
                        out,
                        "\n{}:{}-{} ({side} side; not in quoted patch):",
                        path.display(),
                        range.start,
                        range.end
                    );
                    write_comment(&mut out, comment, comment_types, &mut emitted);
                }
            }
        }
    }
    let has_notes = session
        .session_notes
        .as_deref()
        .is_some_and(|notes| !notes.trim().is_empty());
    if emitted.is_empty() && !has_notes {
        if !all_commit_files_reviewed(session, diff_files, &paths) {
            return Err(TuicrError::NoComments);
        }
        let _ = writeln!(out, "\n{}", reviewed_by_identity(&repo.config()?)?);
    }
    Ok(out)
}

/// Every actual changed file must be explicitly marked reviewed in the current
/// diff. Hidden/ignored files still count, and hunk marks are not file approval.
fn all_commit_files_reviewed(
    session: &ReviewSession,
    diff_files: &[DiffFile],
    paths: &HashSet<std::path::PathBuf>,
) -> bool {
    !paths.is_empty()
        && paths.iter().all(|path| {
            let Some(file) = diff_files.iter().find(|file| file.display_path() == path) else {
                return false;
            };
            session
                .files
                .get(path)
                .is_some_and(|review| review.is_content_reviewed(file.content_hash))
        })
}

fn reviewed_by_identity(config: &git2::Config) -> Result<String> {
    let configured = |key| {
        config
            .get_string(key)
            .ok()
            .filter(|value| !value.trim().is_empty() && !value.contains(['\n', '\r']))
    };
    match (configured("user.name"), configured("user.email")) {
        (Some(name), Some(email)) => Ok(format!("Reviewed-by: {} <{}>", name.trim(), email.trim())),
        _ => Err(TuicrError::InvalidInput(
            "Configure Git user.name and user.email before exporting Reviewed-by tags".into(),
        )),
    }
}

fn quote(out: &mut String, line: &str) {
    let _ = writeln!(out, "> {line}");
}

fn write_file_comments(
    out: &mut String,
    review: &FileReview,
    commit: &str,
    types: &[CommentTypeDefinition],
    emitted: &mut HashSet<String>,
) {
    for c in &review.file_comments {
        if c.commit_id.as_deref().is_none_or(|id| id == commit) {
            write_comment(out, c, types, emitted);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn write_excerpt(
    out: &mut String,
    rows: &[QuotedLine],
    headers: &[String],
    review: Option<&FileReview>,
    commit: &str,
    types: &[CommentTypeDefinition],
    emitted: &mut HashSet<String>,
    keep_all: bool,
) {
    let mut keep = vec![keep_all; rows.len()];
    if review.is_some_and(|r| {
        r.file_comments
            .iter()
            .any(|c| c.commit_id.as_deref().is_none_or(|id| id == commit))
    }) {
        let end = CONTEXT.min(keep.len());
        keep[..end].fill(true);
    }
    let mut insertions: BTreeMap<usize, Vec<&Comment>> = BTreeMap::new();
    if let Some(review) = review {
        let mut comments: Vec<_> = review.line_comments.iter().collect();
        comments.sort_by_key(|(line, _)| *line);
        for (line, comments) in comments {
            for c in comments {
                if c.commit_id.as_deref().is_some_and(|id| id != commit) {
                    continue;
                }
                let range = c.line_range.unwrap_or(LineRange::single(*line));
                let number = |row: &QuotedLine| {
                    if c.side == Some(LineSide::Old) {
                        row.old
                    } else {
                        row.new
                    }
                };
                if let Some(end) = rows.iter().position(|r| number(r) == Some(range.end)) {
                    let start = rows
                        .iter()
                        .position(|r| number(r) == Some(range.start))
                        .unwrap_or(end);
                    let lo = start.saturating_sub(CONTEXT);
                    let hi = (end + CONTEXT + 1).min(rows.len());
                    keep[lo..hi].fill(true);
                    insertions.entry(end).or_default().push(c);
                }
            }
        }
    }
    let mut skipping = false;
    let mut previous_hunk = None;
    for (i, row) in rows.iter().enumerate() {
        if !keep[i] {
            if !skipping {
                quote(out, "[snip]");
            }
            skipping = true;
            continue;
        }
        if previous_hunk != Some(row.hunk) {
            if previous_hunk.is_some() && !skipping {
                quote(out, "[snip]");
            }
            if let Some(header) = headers.get(row.hunk) {
                quote(out, header);
            }
            previous_hunk = Some(row.hunk);
        }
        skipping = false;
        quote(out, &row.text);
        if let Some(comments) = insertions.get(&i) {
            for c in comments {
                write_comment(out, c, types, emitted);
            }
        }
    }
}

fn write_comment(
    out: &mut String,
    comment: &Comment,
    types: &[CommentTypeDefinition],
    emitted: &mut HashSet<String>,
) {
    out.push('\n');
    let label = super::markdown::export_comment_type_label(&comment.comment_type, types);
    if !label.is_empty() {
        let _ = writeln!(out, "[{label}]");
    }
    write_wrapped(out, &comment.content);
    out.push('\n');
    emitted.insert(comment.id.clone());
}

/// Preserve paragraphs, explicit line breaks and code indentation. Long words
/// (notably URLs) and preformatted code are deliberately not broken.
fn write_wrapped(out: &mut String, text: &str) {
    let mut fenced = false;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~") {
            fenced = !fenced;
            let _ = writeln!(out, "{line}");
        } else if fenced
            || line.starts_with("    ")
            || line.starts_with('\t')
            || line.starts_with("Reviewed-by: ")
        {
            let _ = writeln!(out, "{line}");
        } else {
            let mut width = 0;
            for word in line.split_whitespace() {
                let len = UnicodeWidthStr::width(word);
                if width > 0 && width + 1 + len > COMMENT_WIDTH {
                    out.push('\n');
                    width = 0;
                }
                if width > 0 {
                    out.push(' ');
                    width += 1;
                }
                out.push_str(word);
                width += len;
            }
            out.push('\n');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CommentType, FilePatch, FileStatus, SessionDiffSource};
    use std::path::PathBuf;

    fn comment(text: &str, side: LineSide) -> Comment {
        Comment::new(text.into(), CommentType::from_id("none"), Some(side))
    }

    fn commit(repo: &Repository, message: &str) -> Oid {
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let signature = git2::Signature::new(
            "Reviewer",
            "review@example.org",
            &git2::Time::new(1700000000, 60),
        )
        .unwrap();
        let parent = repo.head().ok().map(|head| head.peel_to_commit().unwrap());
        let parents: Vec<_> = parent.iter().collect();
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &parents,
        )
        .unwrap()
    }

    fn files_for_commit(repo: &Repository, id: Oid) -> Vec<DiffFile> {
        let commit = repo.find_commit(id).unwrap();
        let tree = commit.tree().unwrap();
        let parent = commit.parent(0).ok().map(|c| c.tree().unwrap());
        let diff = repo
            .diff_tree_to_tree(parent.as_ref(), Some(&tree), None)
            .unwrap();
        let patches = diff
            .deltas()
            .enumerate()
            .map(|(index, delta)| {
                let mut patch = Patch::from_diff(&diff, index).unwrap().unwrap();
                let raw = patch.to_buf().unwrap();
                FilePatch::new(
                    delta.old_file().path().map(Path::to_path_buf),
                    delta.new_file().path().map(Path::to_path_buf),
                    match delta.status() {
                        git2::Delta::Added => FileStatus::Added,
                        git2::Delta::Deleted => FileStatus::Deleted,
                        _ => FileStatus::Modified,
                    },
                    String::from_utf8_lossy(&raw).to_string(),
                )
            })
            .collect();
        crate::vcs::diff_parser::parse_file_patches(
            patches,
            &crate::syntax::SyntaxHighlighter::default(),
        )
        .unwrap()
    }

    struct Fixture {
        dir: tempfile::TempDir,
        selected: CommitInfo,
        session: ReviewSession,
        diff_files: Vec<DiffFile>,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let repo = Repository::init(dir.path()).unwrap();
            let original = (1..=150)
                .map(|n| format!("original {n}\n"))
                .collect::<String>();
            std::fs::write(dir.path().join("test.txt"), &original).unwrap();
            commit(&repo, "Base");
            let updated = original
                .replace("original 30\n", "changed 30\n")
                .replace("original 120\n", "changed 120\n");
            std::fs::write(dir.path().join("test.txt"), updated).unwrap();
            let id = commit(&repo, "Improve the file\n\nExplain the motivation.\n");
            let diff_files = files_for_commit(&repo, id);
            let selected = CommitInfo {
                id: id.to_string(),
                short_id: id.to_string()[..7].into(),
                change_id_prefix_len: None,
                branch_name: None,
                summary: "Improve the file".into(),
                body: Some("Explain the motivation.".into()),
                author: "Reviewer".into(),
                time: chrono::Utc::now(),
            };
            let mut session = ReviewSession::new(
                dir.path().into(),
                id.to_string(),
                None,
                SessionDiffSource::CommitRange,
            );
            for file in &diff_files {
                session.add_diff_file(file);
            }
            Self {
                dir,
                selected,
                session,
                diff_files,
            }
        }

        fn review(&mut self) -> &mut FileReview {
            self.session.files.get_mut(Path::new("test.txt")).unwrap()
        }

        fn export(&self) -> Result<String> {
            generate_mail_content(
                self.dir.path(),
                &self.selected,
                &self.session,
                &[],
                &self.diff_files,
            )
        }
    }

    #[test]
    fn quotes_real_commit_and_inserts_both_sides_with_ten_lines_of_context() {
        let mut f = Fixture::new();
        f.review()
            .add_line_comment(30, comment("Why remove this?", LineSide::Old));
        f.review()
            .add_line_comment(30, comment("Handle the error here.", LineSide::New));
        // A dirty checkout must not change the exported patch.
        std::fs::write(f.dir.path().join("test.txt"), "uncommitted content").unwrap();
        let out = f.export().unwrap();
        assert!(out.starts_with("> Improve the file\n> \n> Explain the motivation."));
        for header in ["> From ", "> From:", "> Date:", "> Subject:"] {
            assert!(!out.contains(header), "{out}");
        }
        assert!(out.contains(
            "> -original 30\n\nWhy remove this?\n\n> +changed 30\n\nHandle the error here."
        ));
        assert!(out.contains(">  original 20\n"));
        assert!(out.contains(">  original 40\n"));
        assert!(!out.contains(">  original 19\n"));
        assert!(!out.contains(">  original 41\n"));
        assert!(!out.contains("changed 120"));
        assert!(!out.contains("uncommitted content"));
        assert!(out.contains("> [snip]\n"));
    }

    #[test]
    fn includes_ranges_and_merges_nearby_windows_without_duplicate_lines() {
        let mut f = Fixture::new();
        let mut range = comment("Explain this range.", LineSide::New);
        range.line_range = Some(LineRange::new(28, 31));
        f.review().add_line_comment(31, range);
        f.review()
            .add_line_comment(33, comment("Nearby comment.", LineSide::New));
        let out = f.export().unwrap();
        assert_eq!(out.matches("> +changed 30\n").count(), 1);
        assert!(out.contains(">  original 31\n\nExplain this range."));
        assert!(out.contains(">  original 33\n\nNearby comment."));
    }

    #[test]
    fn filters_other_commits_and_errors_if_nothing_is_exportable() {
        let mut f = Fixture::new();
        f.review().add_line_comment(
            30,
            comment("Other commit.", LineSide::New).with_commit_id("different"),
        );
        assert!(matches!(f.export(), Err(TuicrError::NoComments)));
        let id = f.selected.id.clone();
        f.review()
            .add_file_comment(comment("This file.", LineSide::New).with_commit_id(id));
        let out = f.export().unwrap();
        assert!(out.contains("This file."));
        assert!(!out.contains("Other commit."));
    }

    #[test]
    fn includes_review_file_commit_message_and_unmapped_comments() {
        let mut f = Fixture::new();
        f.session
            .review_comments
            .push(comment("Overall review.", LineSide::New));
        f.review()
            .add_file_comment(comment("File-level review.", LineSide::New));
        f.review()
            .add_line_comment(999, comment("Missing anchor.", LineSide::Old));
        let path = PathBuf::from(format!("Commit Message ({})", f.selected.short_id));
        let mut message_review = FileReview::new(path.clone(), FileStatus::Added, 0);
        message_review.add_line_comment(3, comment("Clarify the motivation.", LineSide::New));
        f.session.files.insert(path, message_review);
        let out = f.export().unwrap();
        assert!(out.starts_with("> Improve the file\n"));
        assert!(out.contains("\nOverall review.\n"));
        assert!(out.contains("File-level review."));
        assert!(out.contains("> Explain the motivation.\n\nClarify the motivation."));
        assert!(out.contains("test.txt:999-999 (old side; not in quoted patch):"));
        assert!(out.contains("Missing anchor."));
    }

    #[test]
    fn comments_in_expanded_context_still_get_inline_excerpts() {
        let mut f = Fixture::new();
        f.review()
            .add_line_comment(75, comment("Unchanged context.", LineSide::New));
        let out = f.export().unwrap();
        assert!(out.contains(">  original 65\n"));
        assert!(out.contains(">  original 75\n\nUnchanged context."));
        assert!(out.contains(">  original 85\n"));
        assert!(!out.contains("not in quoted patch"));
        assert!(!out.contains("changed 30"));
    }

    #[test]
    fn handles_rename_metadata_and_file_comments_without_hunks() {
        let mut f = Fixture::new();
        std::fs::rename(
            f.dir.path().join("test.txt"),
            f.dir.path().join("renamed.txt"),
        )
        .unwrap();
        let repo = Repository::open(f.dir.path()).unwrap();
        f.selected.id = commit(&repo, "Rename file").to_string();
        let mut review = FileReview::new("renamed.txt".into(), FileStatus::Renamed, 0);
        review.add_file_comment(comment("Why rename this?", LineSide::New));
        f.session.files.insert("renamed.txt".into(), review);
        let out = f.export().unwrap();
        assert!(out.contains("> rename from test.txt\n"), "{out}");
        assert!(out.contains("> rename to renamed.txt\n"));
        assert!(out.contains("Why rename this?"));
    }

    #[test]
    fn handles_deleted_files_and_binary_file_comments() {
        let mut f = Fixture::new();
        std::fs::remove_file(f.dir.path().join("test.txt")).unwrap();
        std::fs::write(f.dir.path().join("data.bin"), [0, 1, 0, 2]).unwrap();
        let repo = Repository::open(f.dir.path()).unwrap();
        f.selected.id = commit(&repo, "Delete text and add binary").to_string();
        f.review()
            .add_line_comment(30, comment("Keep this line?", LineSide::Old));
        let mut binary = FileReview::new("data.bin".into(), FileStatus::Added, 0);
        binary.add_file_comment(comment("Document this binary.", LineSide::New));
        f.session.files.insert("data.bin".into(), binary);
        let out = f.export().unwrap();
        assert!(out.contains("> +++ /dev/null\n"));
        assert!(out.contains("> -changed 30\n\nKeep this line?"));
        assert!(out.contains("Binary files"));
        assert!(out.contains("Document this binary."));
    }

    #[test]
    fn does_not_pull_unscoped_comments_from_unrelated_files_into_this_commit() {
        let mut f = Fixture::new();
        let mut other = FileReview::new("unrelated.txt".into(), FileStatus::Modified, 0);
        other.add_file_comment(comment("Unrelated comment.", LineSide::New));
        f.session.files.insert("unrelated.txt".into(), other);
        assert!(matches!(f.export(), Err(TuicrError::NoComments)));
    }

    fn configure_reviewer(f: &Fixture) {
        let repo = Repository::open(f.dir.path()).unwrap();
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Configured Reviewer").unwrap();
        config
            .set_str("user.email", "configured@example.org")
            .unwrap();
    }

    #[test]
    fn reviewed_file_exports_one_commit_trailer_using_git_config_identity() {
        let mut f = Fixture::new();
        configure_reviewer(&f);
        f.review().reviewed = true;
        let out = f.export().unwrap();
        assert!(out.starts_with("> Improve the file\n"));
        assert!(
            out.ends_with("\nReviewed-by: Configured Reviewer <configured@example.org>\n"),
            "{out}"
        );
        assert_eq!(out.matches("Reviewed-by:").count(), 1);
        assert_eq!(
            f.review().comment_count(),
            0,
            "export must not persist the tag"
        );
    }

    #[test]
    fn hunk_marks_do_not_substitute_for_file_approval() {
        let mut f = Fixture::new();
        configure_reviewer(&f);
        let keys = f.diff_files[0].hunk_review_keys();
        assert_eq!(keys.len(), 2);
        f.review().reviewed_hunks.insert(keys[0].clone());
        assert!(matches!(f.export(), Err(TuicrError::NoComments)));
        f.review().reviewed_hunks.insert(keys[1].clone());
        assert!(matches!(f.export(), Err(TuicrError::NoComments)));
        f.review().reviewed = true;
        assert_eq!(f.export().unwrap().matches("Reviewed-by:").count(), 1);
    }

    #[test]
    fn stale_and_unrelated_review_marks_do_not_endorse_the_selected_commit() {
        let mut f = Fixture::new();
        f.review().reviewed = true;
        f.review().content_hash = Some(0);
        f.review().reviewed_hunks.insert("stale hunk key".into());
        assert!(matches!(f.export(), Err(TuicrError::NoComments)));
        let mut unrelated = FileReview::new("unrelated.txt".into(), FileStatus::Modified, 0);
        unrelated.reviewed = true;
        f.session.files.insert("unrelated.txt".into(), unrelated);
        assert!(matches!(f.export(), Err(TuicrError::NoComments)));
    }

    #[test]
    fn review_tags_require_identity_only_when_emitting_a_trailer() {
        let mut f = Fixture::new();
        configure_reviewer(&f);
        let repo = Repository::open(f.dir.path()).unwrap();
        let mut config = repo.config().unwrap();
        config.set_str("user.email", "").unwrap();
        f.review().reviewed = true;
        let error = f.export().unwrap_err().to_string();
        assert!(error.contains("user.name and user.email"), "{error}");
        // A comment suppresses endorsement even when every file is reviewed,
        // so identity is not required for ordinary comment exports.
        f.review()
            .add_file_comment(comment("A comment.", LineSide::New));
        let out = f.export().unwrap();
        assert!(out.contains("A comment."));
        assert!(!out.contains("Reviewed-by:"));
        f.review().file_comments.clear();
        config
            .set_str("user.email", "configured@example.org")
            .unwrap();
        config.set_str("user.name", "").unwrap();
        assert!(matches!(f.export(), Err(TuicrError::InvalidInput(_))));
    }

    #[test]
    fn reviewed_deleted_file_allows_a_commit_endorsement() {
        let mut f = Fixture::new();
        configure_reviewer(&f);
        std::fs::remove_file(f.dir.path().join("test.txt")).unwrap();
        let repo = Repository::open(f.dir.path()).unwrap();
        let id = commit(&repo, "Delete file");
        f.selected.id = id.to_string();
        f.diff_files = files_for_commit(&repo, id);
        f.session.add_diff_file(&f.diff_files[0]);
        f.review().reviewed = true;
        let out = f.export().unwrap();
        assert!(
            out.ends_with("\nReviewed-by: Configured Reviewer <configured@example.org>\n"),
            "{out}"
        );
    }

    #[test]
    fn all_actual_commit_files_must_be_reviewed_even_if_filtered_out() {
        let mut f = Fixture::new();
        configure_reviewer(&f);
        std::fs::write(f.dir.path().join("second.txt"), "another file\n").unwrap();
        std::fs::write(f.dir.path().join("test.txt"), "updated again\n").unwrap();
        let repo = Repository::open(f.dir.path()).unwrap();
        let id = commit(&repo, "Change two files");
        f.selected.id = id.to_string();
        f.diff_files = files_for_commit(&repo, id);
        assert_eq!(f.diff_files.len(), 2);
        for file in &f.diff_files {
            f.session.add_diff_file(file);
        }
        f.review().reviewed = true;
        assert!(matches!(f.export(), Err(TuicrError::NoComments)));
        f.session
            .files
            .get_mut(Path::new("second.txt"))
            .unwrap()
            .reviewed = true;
        assert_eq!(f.export().unwrap().matches("Reviewed-by:").count(), 1);
        // A .tuicrignore-filtered file cannot be certified from stale session state.
        f.diff_files
            .retain(|file| file.display_path() != Path::new("second.txt"));
        assert!(matches!(f.export(), Err(TuicrError::NoComments)));
    }

    #[test]
    fn any_comment_or_review_note_on_this_commit_suppresses_endorsement() {
        let mut f = Fixture::new();
        configure_reviewer(&f);
        f.review().reviewed = true;
        let clean = f.session.clone();
        for kind in 0..6 {
            f.session = clean.clone();
            let note = comment("Please change this.", LineSide::New);
            match kind {
                0 => f.review().add_file_comment(note),
                1 => f.review().add_line_comment(30, note),
                2 => f.session.review_comments.push(note),
                3 => {
                    let path = PathBuf::from(format!("Commit Message ({})", f.selected.short_id));
                    let mut review = FileReview::new(path.clone(), FileStatus::Added, 0);
                    review.add_line_comment(1, note);
                    f.session.files.insert(path, review);
                }
                4 => f.session.session_notes = Some(note.content),
                5 => {
                    let path = PathBuf::from("outside-patch.txt");
                    let mut review = FileReview::new(path.clone(), FileStatus::Modified, 0);
                    review.add_line_comment(999, note.with_commit_id(f.selected.id.clone()));
                    f.session.files.insert(path, review);
                }
                _ => unreachable!(),
            }
            let out = f.export().unwrap();
            assert!(out.contains("Please change this."), "kind {kind}: {out}");
            assert!(!out.contains("Reviewed-by:"), "kind {kind}: {out}");
        }
    }

    #[test]
    fn comments_from_other_commits_do_not_suppress_endorsement() {
        let mut f = Fixture::new();
        configure_reviewer(&f);
        f.review().reviewed = true;
        f.review()
            .add_file_comment(comment("Other commit.", LineSide::New).with_commit_id("other"));
        f.session
            .review_comments
            .push(comment("Other review.", LineSide::New).with_commit_id("other"));
        let mut unrelated = FileReview::new("unrelated.txt".into(), FileStatus::Modified, 0);
        unrelated.add_file_comment(comment("Unrelated.", LineSide::New));
        f.session.files.insert("unrelated.txt".into(), unrelated);
        let out = f.export().unwrap();
        assert_eq!(out.matches("Reviewed-by:").count(), 1);
        assert!(!out.contains("Other commit."));
        assert!(!out.contains("Other review."));
        assert!(!out.contains("Unrelated."));
    }

    #[test]
    fn empty_commits_do_not_receive_vacuous_endorsements() {
        let mut f = Fixture::new();
        f.review().reviewed = true;
        let repo = Repository::open(f.dir.path()).unwrap();
        f.selected.id = commit(&repo, "Empty commit").to_string();
        f.diff_files.clear();
        assert!(matches!(f.export(), Err(TuicrError::NoComments)));
    }

    #[test]
    fn reviewed_by_trailers_are_not_line_wrapped() {
        let mut out = String::new();
        let trailer = format!(
            "Reviewed-by: {} <reviewer@example.org>",
            "Long Name ".repeat(10)
        );
        write_wrapped(&mut out, &trailer);
        assert_eq!(out, format!("{trailer}\n"));
    }

    #[test]
    fn wraps_prose_at_72_display_columns_preserving_paragraphs_code_and_urls() {
        let mut out = String::new();
        let prose = "words to wrap ".repeat(20);
        let code = format!("    {}", "x".repeat(100));
        let url = format!("https://example.org/{}", "x".repeat(100));
        write_wrapped(
            &mut out,
            &format!("{prose}\n\n{code}\n```rust\n{}\n```\n{url}", "y".repeat(90)),
        );
        let paragraphs: Vec<_> = out.split("\n\n").collect();
        assert_eq!(paragraphs.len(), 2);
        assert!(
            paragraphs[0]
                .lines()
                .all(|l| UnicodeWidthStr::width(l) <= 72)
        );
        assert_eq!(
            paragraphs[0].split_whitespace().collect::<Vec<_>>(),
            prose.split_whitespace().collect::<Vec<_>>()
        );
        assert!(out.contains(&code));
        assert!(out.contains(&url));
        assert!(out.contains(&format!("```rust\n{}\n```", "y".repeat(90))));
    }
}
