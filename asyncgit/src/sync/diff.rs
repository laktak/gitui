//! sync git api for fetching a diff
//!
//! Inline word-level highlighting is computed via the `similar` crate
//! by pairing consecutive Delete/Add lines within each hunk.

use super::{
	commit_files::{
		get_commit_diff, get_compare_commits_diff, OldNew,
	},
	utils::{get_head_repo, work_dir},
	CommitId, RepoPath,
};
use crate::{
	error::Error,
	error::Result,
	hash,
	sync::{get_stashes, repository::repo},
};
use easy_cast::Conv;
use git2::{
	Delta, Diff, DiffDelta, DiffFormat, DiffHunk, Patch, Repository,
};
use scopetime::scope_time;
use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};
use std::{cell::RefCell, fs, path::Path, rc::Rc};

/// type of diff of a single line
#[derive(Copy, Clone, Default, PartialEq, Eq, Hash, Debug)]
pub enum DiffLineType {
	/// just surrounding line, no change
	#[default]
	None,
	/// header of the hunk
	Header,
	/// line added
	Add,
	/// line deleted
	Delete,
}

impl From<git2::DiffLineType> for DiffLineType {
	fn from(line_type: git2::DiffLineType) -> Self {
		match line_type {
			git2::DiffLineType::HunkHeader => Self::Header,
			git2::DiffLineType::DeleteEOFNL
			| git2::DiffLineType::Deletion => Self::Delete,
			git2::DiffLineType::AddEOFNL
			| git2::DiffLineType::Addition => Self::Add,
			_ => Self::None,
		}
	}
}

/// A byte range [start, end) within a `DiffLine`'s content that was changed.
/// Used to highlight individual changed words on Add/Delete lines.
#[derive(Clone, Hash, Debug, PartialEq, Eq)]
pub struct InlineHighlight {
	/// Byte offset of the start of the changed token (inclusive)
	pub start: usize,
	/// Byte offset of the end of the changed token (exclusive)
	pub end: usize,
}

///
#[derive(Default, Clone, Hash, Debug)]
pub struct DiffLine {
	///
	pub content: Box<str>,
	///
	pub line_type: DiffLineType,
	///
	pub position: DiffLinePosition,
	/// Word-level highlighted byte ranges within `content`.
	/// Non-empty only for Add/Delete lines that have a paired counterpart.
	pub inline_highlights: Vec<InlineHighlight>,
}

///
#[derive(Clone, Copy, Default, Hash, Debug, PartialEq, Eq)]
pub struct DiffLinePosition {
	///
	pub old_lineno: Option<u32>,
	///
	pub new_lineno: Option<u32>,
}

impl PartialEq<&git2::DiffLine<'_>> for DiffLinePosition {
	fn eq(&self, other: &&git2::DiffLine) -> bool {
		other.new_lineno() == self.new_lineno
			&& other.old_lineno() == self.old_lineno
	}
}

impl From<&git2::DiffLine<'_>> for DiffLinePosition {
	fn from(line: &git2::DiffLine<'_>) -> Self {
		Self {
			old_lineno: line.old_lineno(),
			new_lineno: line.new_lineno(),
		}
	}
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Hash)]
pub(crate) struct HunkHeader {
	pub old_start: u32,
	pub old_lines: u32,
	pub new_start: u32,
	pub new_lines: u32,
}

impl From<DiffHunk<'_>> for HunkHeader {
	fn from(h: DiffHunk) -> Self {
		Self {
			old_start: h.old_start(),
			old_lines: h.old_lines(),
			new_start: h.new_start(),
			new_lines: h.new_lines(),
		}
	}
}

/// single diff hunk
#[derive(Default, Clone, Hash, Debug)]
pub struct Hunk {
	/// hash of the hunk header
	pub header_hash: u64,
	/// list of `DiffLine`s
	pub lines: Vec<DiffLine>,
}

/// collection of hunks, sum of all diff lines
#[derive(Default, Clone, Hash, Debug)]
pub struct FileDiff {
	/// list of hunks
	pub hunks: Vec<Hunk>,
	/// lines total summed up over hunks
	pub lines: usize,
	///
	pub untracked: bool,
	/// old and new file size in bytes
	pub sizes: (u64, u64),
	/// size delta in bytes
	pub size_delta: i64,
}

/// see <https://libgit2.org/libgit2/#HEAD/type/git_diff_options>
#[derive(
	Debug, Hash, Clone, Copy, PartialEq, Eq, Serialize, Deserialize,
)]
pub struct DiffOptions {
	/// see <https://libgit2.org/libgit2/#HEAD/type/git_diff_options>
	pub ignore_whitespace: bool,
	/// see <https://libgit2.org/libgit2/#HEAD/type/git_diff_options>
	pub context: u32,
	/// see <https://libgit2.org/libgit2/#HEAD/type/git_diff_options>
	pub interhunk_lines: u32,
}

impl Default for DiffOptions {
	fn default() -> Self {
		Self {
			ignore_whitespace: false,
			context: 3,
			interhunk_lines: 0,
		}
	}
}

pub(crate) fn get_diff_raw<'a>(
	repo: &'a Repository,
	p: &str,
	stage: bool,
	reverse: bool,
	options: Option<DiffOptions>,
) -> Result<Diff<'a>> {
	// scope_time!("get_diff_raw");

	let mut opt = git2::DiffOptions::new();
	if let Some(options) = options {
		opt.context_lines(options.context);
		opt.ignore_whitespace(options.ignore_whitespace);
		opt.interhunk_lines(options.interhunk_lines);
	}
	opt.pathspec(p);
	opt.reverse(reverse);

	let diff = if stage {
		// diff against head
		if let Ok(id) = get_head_repo(repo) {
			let parent = repo.find_commit(id.into())?;

			let tree = parent.tree()?;
			repo.diff_tree_to_index(
				Some(&tree),
				Some(&repo.index()?),
				Some(&mut opt),
			)?
		} else {
			repo.diff_tree_to_index(
				None,
				Some(&repo.index()?),
				Some(&mut opt),
			)?
		}
	} else {
		opt.include_untracked(true);
		opt.recurse_untracked_dirs(true);
		repo.diff_index_to_workdir(None, Some(&mut opt))?
	};

	Ok(diff)
}

/// returns diff of a specific file either in `stage` or workdir
pub fn get_diff(
	repo_path: &RepoPath,
	p: &str,
	stage: bool,
	options: Option<DiffOptions>,
) -> Result<FileDiff> {
	scope_time!("get_diff");

	let repo = repo(repo_path)?;
	let work_dir = work_dir(&repo)?;
	let diff = get_diff_raw(&repo, p, stage, false, options)?;

	raw_diff_to_file_diff(&diff, work_dir)
}

/// returns diff of a specific file inside a commit
/// see `get_commit_diff`
pub fn get_diff_commit(
	repo_path: &RepoPath,
	id: CommitId,
	p: String,
	options: Option<DiffOptions>,
) -> Result<FileDiff> {
	scope_time!("get_diff_commit");

	let repo = repo(repo_path)?;
	let work_dir = work_dir(&repo)?;
	let diff = get_commit_diff(
		&repo,
		id,
		Some(p),
		options,
		Some(&get_stashes(repo_path)?.into_iter().collect()),
	)?;

	raw_diff_to_file_diff(&diff, work_dir)
}

/// get file changes of a diff between two commits
pub fn get_diff_commits(
	repo_path: &RepoPath,
	ids: OldNew<CommitId>,
	p: String,
	options: Option<DiffOptions>,
) -> Result<FileDiff> {
	scope_time!("get_diff_commits");

	let repo = repo(repo_path)?;
	let work_dir = work_dir(&repo)?;
	let diff =
		get_compare_commits_diff(&repo, ids, Some(p), options)?;

	raw_diff_to_file_diff(&diff, work_dir)
}

///
//TODO: refactor into helper type with the inline closures as dedicated functions
#[allow(clippy::too_many_lines)]
fn raw_diff_to_file_diff(
	diff: &Diff,
	work_dir: &Path,
) -> Result<FileDiff> {
	let res = Rc::new(RefCell::new(FileDiff::default()));
	{
		let mut current_lines = Vec::new();
		let mut current_hunk: Option<HunkHeader> = None;

		let res_cell = Rc::clone(&res);
		let adder = move |header: &HunkHeader,
		                  lines: &Vec<DiffLine>| {
			let mut res = res_cell.borrow_mut();
			res.hunks.push(Hunk {
				header_hash: hash(header),
				lines: lines.clone(),
			});
			res.lines += lines.len();
		};

		let res_cell = Rc::clone(&res);
		let mut put = |delta: DiffDelta,
		               hunk: Option<DiffHunk>,
		               line: git2::DiffLine| {
			{
				let mut res = res_cell.borrow_mut();
				res.sizes = (
					delta.old_file().size(),
					delta.new_file().size(),
				);
				//TODO: use try_conv
				res.size_delta = (i64::conv(res.sizes.1))
					.saturating_sub(i64::conv(res.sizes.0));
			}
			if let Some(hunk) = hunk {
				let hunk_header = HunkHeader::from(hunk);

				match current_hunk {
					None => current_hunk = Some(hunk_header),
					Some(h) => {
						if h != hunk_header {
							adder(&h, &current_lines);
							current_lines.clear();
							current_hunk = Some(hunk_header);
						}
					}
				}

				let diff_line = DiffLine {
					position: DiffLinePosition::from(&line),
					content: String::from_utf8_lossy(line.content())
						//Note: trim await trailing newline characters
						.trim_matches(is_newline)
						.into(),
					line_type: line.origin_value().into(),
					inline_highlights: Vec::new(),
				};

				current_lines.push(diff_line);
			}
		};

		let new_file_diff = if diff.deltas().len() == 1 {
			if let Some(delta) = diff.deltas().next() {
				if delta.status() == Delta::Untracked {
					let relative_path =
						delta.new_file().path().ok_or_else(|| {
							Error::Generic(
								"new file path is unspecified."
									.to_string(),
							)
						})?;

					let newfile_path = work_dir.join(relative_path);

					if let Some(newfile_content) =
						new_file_content(&newfile_path)
					{
						let mut patch = Patch::from_buffers(
							&[],
							None,
							newfile_content.as_slice(),
							Some(&newfile_path),
							None,
						)?;

						patch.print(
							&mut |delta,
							      hunk: Option<DiffHunk>,
							      line: git2::DiffLine| {
								put(delta, hunk, line);
								true
							},
						)?;

						true
					} else {
						false
					}
				} else {
					false
				}
			} else {
				false
			}
		} else {
			false
		};

		if !new_file_diff {
			diff.print(
				DiffFormat::Patch,
				move |delta, hunk, line: git2::DiffLine| {
					put(delta, hunk, line);
					true
				},
			)?;
		}

		if !current_lines.is_empty() {
			adder(
				&current_hunk.map_or_else(
					|| Err(Error::Generic("invalid hunk".to_owned())),
					Ok,
				)?,
				&current_lines,
			);
		}

		if new_file_diff {
			res.borrow_mut().untracked = true;
		}
	}
	let mut result = Rc::try_unwrap(res)
		.map_err(|_| Error::Generic("rc unwrap error".to_owned()))?
		.into_inner();

	for hunk in &mut result.hunks {
		add_inline_highlights(&mut hunk.lines);
	}

	Ok(result)
}

const fn is_newline(c: char) -> bool {
	c == '\n' || c == '\r'
}

/// For each contiguous group of Delete lines followed immediately by Add lines
/// in a hunk, compute word-level highlights using `similar`.
///
/// Rules:
/// - If the group has **equal** numbers of deletes and adds (N:N), pair each
///   Delete with the corresponding Add (first-to-first, etc.) and annotate
///   both with changed-word byte ranges.
/// - If the counts differ (N:M where N≠M), pair them using a greedy best-match
///   similarity algorithm (requiring similarity ratio >= 0.5) so that similar
///   lines are still highlighted, while completely different lines are skipped.
fn add_inline_highlights(lines: &mut [DiffLine]) {
	let mut i = 0;
	while i < lines.len() {
		// Only enter a group at a Delete line
		if lines[i].line_type != DiffLineType::Delete {
			i += 1;
			continue;
		}

		// Collect the full run of consecutive Deletes
		let del_start = i;
		while i < lines.len() && lines[i].line_type == DiffLineType::Delete {
			i += 1;
		}
		let n_del = i - del_start;

		// Collect the full run of consecutive Adds immediately following
		let add_start = i;
		while i < lines.len() && lines[i].line_type == DiffLineType::Add {
			i += 1;
		}
		let n_add = i - add_start;

		// Only annotate when we have both deletes and adds
		if n_del == 0 || n_add == 0 {
			continue;
		}

		if n_del == n_add {
			for k in 0..n_del {
				compute_pair_highlights(lines, del_start + k, add_start + k);
			}
		} else {
			// For N:M (where N != M), pair lines using greedy similarity matching.
			let mut candidates = Vec::new();
			for d_idx in del_start..add_start {
				for a_idx in add_start..i {
					let ratio = calculate_similarity_ratio(
						lines[d_idx].content.as_ref(),
						lines[a_idx].content.as_ref(),
					);
					if ratio >= 0.5 {
						candidates.push((ratio, d_idx, a_idx));
					}
				}
			}

			// Sort candidates by ratio in descending order.
			candidates.sort_by(|a, b| b.0.total_cmp(&a.0));

			let mut paired_d = vec![false; n_del];
			let mut paired_a = vec![false; n_add];

			for (_, d_idx, a_idx) in candidates {
				let d_offset = d_idx - del_start;
				let a_offset = a_idx - add_start;
				if !paired_d[d_offset] && !paired_a[a_offset] {
					compute_pair_highlights(lines, d_idx, a_idx);
					paired_d[d_offset] = true;
					paired_a[a_offset] = true;
				}
			}
		}
	}
}

fn calculate_similarity_ratio(old: &str, new: &str) -> f64 {
	let old_tokens = tokenize_code(old);
	let new_tokens = tokenize_code(new);
	let diff = TextDiff::from_slices(&old_tokens, &new_tokens);
	let mut equal_len = 0;
	for change in diff.iter_all_changes() {
		if change.tag() == ChangeTag::Equal {
			equal_len += change.value().len();
		}
	}
	let total_len = old.len() + new.len();
	if total_len == 0 {
		1.0
	} else {
		(2.0 * equal_len as f64) / (total_len as f64)
	}
}

/// Compute word-level diff between `lines[del_idx]` (Delete) and
/// `lines[add_idx]` (Add) and store the resulting byte ranges in each.
fn compute_pair_highlights(lines: &mut [DiffLine], del_idx: usize, add_idx: usize) {
	let old_content = lines[del_idx].content.clone();
	let new_content = lines[add_idx].content.clone();

	let old_tokens = tokenize_code(old_content.as_ref());
	let new_tokens = tokenize_code(new_content.as_ref());
	let diff = TextDiff::from_slices(&old_tokens, &new_tokens);

	let mut del_highlights: Vec<InlineHighlight> = Vec::new();
	let mut add_highlights: Vec<InlineHighlight> = Vec::new();
	let mut del_offset: usize = 0;
	let mut add_offset: usize = 0;

	for change in diff.iter_all_changes() {
		let token: &str = change.value();
		let len = token.len();
		match change.tag() {
			ChangeTag::Delete => {
				del_highlights.push(InlineHighlight {
					start: del_offset,
					end: del_offset + len,
				});
				del_offset += len;
			}
			ChangeTag::Insert => {
				add_highlights.push(InlineHighlight {
					start: add_offset,
					end: add_offset + len,
				});
				add_offset += len;
			}
			ChangeTag::Equal => {
				del_offset += len;
				add_offset += len;
			}
		}
	}

	merge_whitespace_separated_highlights(&mut del_highlights, old_content.as_ref());
	merge_whitespace_separated_highlights(&mut add_highlights, new_content.as_ref());

	lines[del_idx].inline_highlights = del_highlights;
	lines[add_idx].inline_highlights = add_highlights;
}

fn tokenize_code(s: &str) -> Vec<&str> {
	let mut tokens = Vec::new();
	let mut chars = s.char_indices().peekable();

	while let Some(&(start_idx, c)) = chars.peek() {
		if c.is_alphanumeric() || c == '_' {
			chars.next();
			while let Some(&(end_idx, next_c)) = chars.peek() {
				if next_c.is_alphanumeric() || next_c == '_' {
					chars.next();
				} else {
					tokens.push(&s[start_idx..end_idx]);
					break;
				}
			}
			if chars.peek().is_none() {
				tokens.push(&s[start_idx..]);
			}
		} else if c.is_whitespace() {
			chars.next();
			while let Some(&(end_idx, next_c)) = chars.peek() {
				if next_c.is_whitespace() {
					chars.next();
				} else {
					tokens.push(&s[start_idx..end_idx]);
					break;
				}
			}
			if chars.peek().is_none() {
				tokens.push(&s[start_idx..]);
			}
		} else {
			chars.next();
			if let Some(&(end_idx, _)) = chars.peek() {
				tokens.push(&s[start_idx..end_idx]);
			} else {
				tokens.push(&s[start_idx..]);
			}
		}
	}
	tokens
}

fn merge_whitespace_separated_highlights(highlights: &mut Vec<InlineHighlight>, content: &str) {
	if highlights.len() <= 1 {
		return;
	}

	let mut merged: Vec<InlineHighlight> = Vec::new();
	for h in highlights.drain(..) {
		if let Some(last) = merged.last_mut() {
			let between = &content[last.end..h.start];
			if last.end == h.start || (!between.is_empty() && between.chars().all(|c| c.is_whitespace())) {
				last.end = h.end;
			} else {
				merged.push(h);
			}
		} else {
			merged.push(h);
		}
	}
	*highlights = merged;
}

fn new_file_content(path: &Path) -> Option<Vec<u8>> {
	if let Ok(meta) = fs::symlink_metadata(path) {
		if meta.file_type().is_symlink() {
			if let Ok(path) = fs::read_link(path) {
				return Some(
					path.to_str()?.to_string().as_bytes().into(),
				);
			}
		} else if !meta.file_type().is_dir() {
			if let Ok(content) = fs::read(path) {
				return Some(content);
			}
		}
	}

	None
}

#[cfg(test)]
mod tests {
	use super::{get_diff, get_diff_commit};
	use crate::{
		error::Result,
		sync::{
			commit, stage_add_file,
			status::{get_status, StatusType},
			tests::{get_statuses, repo_init, repo_init_empty},
			RepoPath,
		},
	};
	use std::{
		fs::{self, File},
		io::Write,
		path::Path,
	};

	#[test]
	fn test_untracked_subfolder() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		assert_eq!(get_statuses(repo_path), (0, 0));

		fs::create_dir(root.join("foo")).unwrap();
		File::create(root.join("foo/bar.txt"))
			.unwrap()
			.write_all(b"test\nfoo")
			.unwrap();

		assert_eq!(get_statuses(repo_path), (1, 0));

		let diff =
			get_diff(repo_path, "foo/bar.txt", false, None).unwrap();

		assert_eq!(diff.hunks.len(), 1);
		assert_eq!(&*diff.hunks[0].lines[1].content, "test");
	}

	#[test]
	fn test_empty_repo() {
		let file_path = Path::new("foo.txt");
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		assert_eq!(get_statuses(repo_path), (0, 0));

		File::create(root.join(file_path))
			.unwrap()
			.write_all(b"test\nfoo")
			.unwrap();

		assert_eq!(get_statuses(repo_path), (1, 0));

		stage_add_file(repo_path, file_path).unwrap();

		assert_eq!(get_statuses(repo_path), (0, 1));

		let diff = get_diff(
			repo_path,
			file_path.to_str().unwrap(),
			true,
			None,
		)
		.unwrap();

		assert_eq!(diff.hunks.len(), 1);
	}

	static HUNK_A: &str = r"
1   start
2
3
4
5
6   middle
7
8
9
0
1   end";

	static HUNK_B: &str = r"
1   start
2   newa
3
4
5
6   middle
7
8
9
0   newb
1   end";

	#[test]
	fn test_hunks() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		assert_eq!(get_statuses(repo_path), (0, 0));

		let file_path = root.join("bar.txt");

		{
			File::create(&file_path)
				.unwrap()
				.write_all(HUNK_A.as_bytes())
				.unwrap();
		}

		let res = get_status(repo_path, StatusType::WorkingDir, None)
			.unwrap();
		assert_eq!(res.len(), 1);
		assert_eq!(res[0].path, "bar.txt");

		stage_add_file(repo_path, Path::new("bar.txt")).unwrap();
		assert_eq!(get_statuses(repo_path), (0, 1));

		// overwrite with next content
		{
			File::create(&file_path)
				.unwrap()
				.write_all(HUNK_B.as_bytes())
				.unwrap();
		}

		assert_eq!(get_statuses(repo_path), (1, 1));

		let res =
			get_diff(repo_path, "bar.txt", false, None).unwrap();

		assert_eq!(res.hunks.len(), 2);
	}

	#[test]
	fn test_diff_newfile_in_sub_dir_current_dir() {
		let file_path = Path::new("foo/foo.txt");
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();

		let sub_path = root.join("foo/");

		fs::create_dir_all(&sub_path).unwrap();
		File::create(root.join(file_path))
			.unwrap()
			.write_all(b"test")
			.unwrap();

		let diff = get_diff(
			&sub_path.to_str().unwrap().into(),
			file_path.to_str().unwrap(),
			false,
			None,
		)
		.unwrap();

		assert_eq!(&*diff.hunks[0].lines[1].content, "test");
	}

	#[test]
	fn test_diff_delta_size() -> Result<()> {
		let file_path = Path::new("bar");
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		File::create(root.join(file_path))?.write_all(b"\x00")?;

		stage_add_file(repo_path, file_path).unwrap();

		commit(repo_path, "commit").unwrap();

		File::create(root.join(file_path))?.write_all(b"\x00\x02")?;

		let diff = get_diff(
			repo_path,
			file_path.to_str().unwrap(),
			false,
			None,
		)
		.unwrap();

		dbg!(&diff);
		assert_eq!(diff.sizes, (1, 2));
		assert_eq!(diff.size_delta, 1);

		Ok(())
	}

	#[test]
	fn test_binary_diff_delta_size_untracked() -> Result<()> {
		let file_path = Path::new("bar");
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		File::create(root.join(file_path))?.write_all(b"\x00\xc7")?;

		let diff = get_diff(
			repo_path,
			file_path.to_str().unwrap(),
			false,
			None,
		)
		.unwrap();

		dbg!(&diff);
		assert_eq!(diff.sizes, (0, 2));
		assert_eq!(diff.size_delta, 2);

		Ok(())
	}

	#[test]
	fn test_diff_delta_size_commit() -> Result<()> {
		let file_path = Path::new("bar");
		let (_td, repo) = repo_init_empty().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		File::create(root.join(file_path))?.write_all(b"\x00")?;

		stage_add_file(repo_path, file_path).unwrap();

		commit(repo_path, "").unwrap();

		File::create(root.join(file_path))?.write_all(b"\x00\x02")?;

		stage_add_file(repo_path, file_path).unwrap();

		let id = commit(repo_path, "").unwrap();

		let diff =
			get_diff_commit(repo_path, id, String::new(), None)
				.unwrap();

		dbg!(&diff);
		assert_eq!(diff.sizes, (1, 2));
		assert_eq!(diff.size_delta, 1);

		Ok(())
	}

	#[test]
	fn test_add_inline_highlights_pairing() {
		use super::{add_inline_highlights, DiffLine, DiffLineType, DiffLinePosition};

		// Case 1: 1 Delete vs 2 Adds where one Add is very similar.
		let mut lines = vec![
			DiffLine {
				content: "// printToolCallLine prints a single line describing the tool call.".into(),
				line_type: DiffLineType::Delete,
				position: DiffLinePosition::default(),
				inline_highlights: Vec::new(),
			},
			DiffLine {
				content: "// printToolCallLine prints a tool call line WITHOUT a trailing newline so that".into(),
				line_type: DiffLineType::Add,
				position: DiffLinePosition::default(),
				inline_highlights: Vec::new(),
			},
			DiffLine {
				content: "// printToolCallDone can append elapsed time on the same line.".into(),
				line_type: DiffLineType::Add,
				position: DiffLinePosition::default(),
				inline_highlights: Vec::new(),
			},
		];

		add_inline_highlights(&mut lines);

		// The delete line and the first add line should be paired and have inline highlights.
		assert!(!lines[0].inline_highlights.is_empty());
		assert!(!lines[1].inline_highlights.is_empty());
		// The second add line is dissimilar and should have no inline highlights.
		assert!(lines[2].inline_highlights.is_empty());

		// Case 2: 4 Deletes vs 1 Add (completely different).
		// None should be highlighted.
		let mut lines2 = vec![
			DiffLine {
				content: "err := huh.NewInput().".into(),
				line_type: DiffLineType::Delete,
				position: DiffLinePosition::default(),
				inline_highlights: Vec::new(),
			},
			DiffLine {
				content: "Prompt(\"fai> \").".into(),
				line_type: DiffLineType::Delete,
				position: DiffLinePosition::default(),
				inline_highlights: Vec::new(),
			},
			DiffLine {
				content: "Value(&input).".into(),
				line_type: DiffLineType::Delete,
				position: DiffLinePosition::default(),
				inline_highlights: Vec::new(),
			},
			DiffLine {
				content: "Run()".into(),
				line_type: DiffLineType::Delete,
				position: DiffLinePosition::default(),
				inline_highlights: Vec::new(),
			},
			DiffLine {
				content: "fmt.Print(fai.AnsiCyan + fai.AnsiBold + \"fai> \" + fai.AnsiReset)".into(),
				line_type: DiffLineType::Add,
				position: DiffLinePosition::default(),
				inline_highlights: Vec::new(),
			},
		];

		add_inline_highlights(&mut lines2);
		assert!(lines2.iter().all(|l| l.inline_highlights.is_empty()));

		// Case 3: Whitespace between two word diff colored tokens should be merged.
		let mut lines3 = vec![
			DiffLine {
				content: "foo bar".into(),
				line_type: DiffLineType::Delete,
				position: DiffLinePosition::default(),
				inline_highlights: Vec::new(),
			},
			DiffLine {
				content: "baz qux".into(),
				line_type: DiffLineType::Add,
				position: DiffLinePosition::default(),
				inline_highlights: Vec::new(),
			},
		];

		add_inline_highlights(&mut lines3);

		// They should have exactly 1 highlight range that spans the entire content.
		assert_eq!(lines3[0].inline_highlights.len(), 1);
		assert_eq!(lines3[0].inline_highlights[0].start, 0);
		assert_eq!(lines3[0].inline_highlights[0].end, 7);

		assert_eq!(lines3[1].inline_highlights.len(), 1);
		assert_eq!(lines3[1].inline_highlights[0].start, 0);
		assert_eq!(lines3[1].inline_highlights[0].end, 7);

		// Case 4: Non-whitespace separated tokens should be tokenized fine-grained.
		let mut lines4 = vec![
			DiffLine {
				content: "artifactVersion=1".into(),
				line_type: DiffLineType::Delete,
				position: DiffLinePosition::default(),
				inline_highlights: Vec::new(),
			},
			DiffLine {
				content: "artifactVersion=2".into(),
				line_type: DiffLineType::Add,
				position: DiffLinePosition::default(),
				inline_highlights: Vec::new(),
			},
		];

		add_inline_highlights(&mut lines4);

		assert_eq!(lines4[0].inline_highlights.len(), 1);
		assert_eq!(lines4[0].inline_highlights[0].start, 16);
		assert_eq!(lines4[0].inline_highlights[0].end, 17);

		assert_eq!(lines4[1].inline_highlights.len(), 1);
		assert_eq!(lines4[1].inline_highlights[0].start, 16);
		assert_eq!(lines4[1].inline_highlights[0].end, 17);
	}
}
