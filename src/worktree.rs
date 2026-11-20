//! Which worktree a session is writing into, and the branch checked out there.

use std::path::{Path, PathBuf};

use crate::{
    basename, clean, head_from_git_dir, safe_session_id, seg, step_dir, Glyphs, Input, Seg,
    WorktreeSession, BRIGHT_CYAN, RESET, YELLOW,
};

/// A `.git` directory is the main tree; a `.git` FILE holds `gitdir: <path>`
/// (relative to `cwd`) pointing at `.git/worktrees/<name>` for a linked worktree,
/// or `.git/modules/<name>` for a submodule.
fn resolve_git_dir(cwd: &str) -> Option<PathBuf> {
    if cwd.trim().is_empty() {
        return None; // never resolve .git relative to the process's own cwd
    }
    let dot_git = Path::new(cwd).join(".git");
    let meta = std::fs::metadata(&dot_git).ok()?;
    if meta.is_dir() {
        return Some(dot_git);
    }
    if !meta.is_file() {
        return None;
    }

    let contents = std::fs::read_to_string(&dot_git).ok()?;
    let line = contents.lines().next().unwrap_or("").trim();
    let target = line.strip_prefix("gitdir:")?.trim();

    let p = Path::new(target);
    Some(if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new(cwd).join(p)
    })
}

fn worktree_name_from_git_dir(git_dir: &Path) -> Option<String> {
    let name = git_dir.file_name()?.to_str()?;
    let parent = git_dir.parent()?.file_name()?.to_str()?;

    if parent == "worktrees" && !name.is_empty() {
        Some(name.to_string())
    } else {
        None
    }
}

/// `git_worktree` from stdin is the WORKTREE NAME, not a branch, so the default
/// resolution reports it as one. Reading HEAD is what yields the real branch.
fn branch_from_cwd(cwd: &str) -> Option<String> {
    head_from_git_dir(&resolve_git_dir(cwd)?)
}

/// When `path` is known the branch is read from THERE: an agent that creates a
/// worktree and checks a branch out in it must stop reporting the base branch.
#[derive(Debug, PartialEq, Eq)]
pub struct WorktreeFocus {
    pub name: String,
    pub path: Option<String>,
}

/// Written by a `PostToolUse` hook as `<step_dir>/claude-worktree-<session_id>`,
/// holding `name\tpath`, because Claude Code sends `worktree.*` only for
/// `--worktree` sessions.
fn worktree_marker(session_id: Option<&str>) -> Option<WorktreeFocus> {
    let sid = safe_session_id(session_id)?;
    let raw = std::fs::read_to_string(step_dir().join(format!("claude-worktree-{sid}"))).ok()?;

    let line = raw.trim_matches(|c| c == '\n' || c == '\r');
    let (name, path) = match line.split_once('\t') {
        Some((name, path)) => (name.trim(), Some(path.trim())),
        None => (line.trim(), None),
    };

    if name.is_empty() {
        return None;
    }

    Some(WorktreeFocus {
        name: name.to_string(),
        path: path.filter(|p| !p.is_empty()).map(str::to_string),
    })
}

/// Its own worktree first (that may sit outside `cwd`), then one a hook saw it
/// writing to, then the worktree `cwd` is inside.
pub fn resolve_worktree(
    cwd: Option<&str>,
    git_worktree: Option<&str>,
    session: Option<&WorktreeSession>,
    session_id: Option<&str>,
) -> Option<WorktreeFocus> {
    fn named(value: Option<&str>) -> Option<&str> {
        value.map(str::trim).filter(|s| !s.is_empty())
    }

    let session_path = session.and_then(|w| named(w.path.as_deref()));

    if let Some(name) = session.and_then(|w| named(w.name.as_deref())) {
        return Some(WorktreeFocus {
            name: name.to_string(),
            path: session_path.map(str::to_string),
        });
    }
    if let Some(path) = session_path {
        return Some(WorktreeFocus {
            name: basename(path),
            path: Some(path.to_string()),
        });
    }
    if let Some(focus) = worktree_marker(session_id) {
        return Some(focus);
    }
    if let Some(name) = named(git_worktree) {
        return Some(WorktreeFocus {
            name: name.to_string(),
            path: None,
        });
    }

    worktree_name_from_git_dir(&resolve_git_dir(cwd?)?)
        .map(|name| WorktreeFocus { name, path: None })
}

/// A linked worktree swaps the glyph and names itself in its own colour, so
/// branch and worktree never read as one string.
pub fn branch_seg(input: &Input, g: &Glyphs) -> Option<Seg> {
    let git_worktree = input
        .workspace
        .as_ref()
        .and_then(|w| w.git_worktree.as_deref());
    let worktree = resolve_worktree(
        input.cwd.as_deref(),
        git_worktree,
        input.worktree.as_ref(),
        input.session_id.as_deref(),
    );

    // The worktree in play owns the branch; only fall back to cwd's HEAD.
    let branch = worktree
        .as_ref()
        .and_then(|w| w.path.as_deref())
        .and_then(branch_from_cwd)
        .or_else(|| input.cwd.as_deref().and_then(branch_from_cwd))?;
    let branch = clean(&branch, 40);

    let Some(focus) = worktree else {
        return Some(Seg::new("branch", YELLOW, g.branch, &branch));
    };

    let name = clean(&focus.name, 24);

    Some(Seg {
        kind: "branch",
        plain: g.worktree.chars().count()
            + 1
            + branch.chars().count()
            + 2
            + g.tree.chars().count()
            + 1
            + name.chars().count(),
        styled: format!(
            "{}{BRIGHT_CYAN} {} {name}{RESET}",
            seg(YELLOW, g.worktree, &branch),
            g.tree
        ),
        compact: Some((
            g.worktree.chars().count() + 1 + branch.chars().count(),
            seg(YELLOW, g.worktree, &branch),
        )),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WorktreeSession;

    #[test]
    fn a_sessions_own_worktree_outranks_the_one_cwd_sits_in() {
        let session = WorktreeSession {
            name: Some("sandbox".into()),
            path: Some("/Users/me/.claude/worktrees/sandbox".into()),
        };
        let focus = |name: &str, path: Option<&str>| {
            Some(WorktreeFocus {
                name: name.to_string(),
                path: path.map(str::to_string),
            })
        };

        // cwd is the main tree, but the session writes into a worktree — and the
        // path travels with it, so the branch can be read from there.
        assert_eq!(
            resolve_worktree(Some("/repo"), None, Some(&session), None),
            focus("sandbox", Some("/Users/me/.claude/worktrees/sandbox"))
        );
        // Nameless session (hook-based): fall back to the path's basename.
        let path_only = WorktreeSession {
            name: None,
            path: Some("/Users/me/claude-workspace/my-repo-feature-x/".into()),
        };
        assert_eq!(
            resolve_worktree(Some("/repo"), None, Some(&path_only), None),
            focus(
                "my-repo-feature-x",
                Some("/Users/me/claude-workspace/my-repo-feature-x/")
            )
        );
        // No session worktree: cwd's own linked worktree is what counts.
        assert_eq!(
            resolve_worktree(Some("/repo"), Some("sandbox-wt"), None, None),
            focus("sandbox-wt", None)
        );
        assert_eq!(
            resolve_worktree(
                Some("/repo"),
                Some("  "),
                Some(&WorktreeSession::default()),
                None
            ),
            None
        );
    }

    #[test]
    fn only_a_worktrees_git_dir_yields_a_worktree_name() {
        assert_eq!(
            worktree_name_from_git_dir(Path::new("/repo/.git/worktrees/sandbox-wt")).as_deref(),
            Some("sandbox-wt")
        );
        // A submodule's pointer is also a .git file, and is not a worktree.
        assert!(worktree_name_from_git_dir(Path::new("/repo/.git/modules/vendor")).is_none());
        assert!(worktree_name_from_git_dir(Path::new("/repo/.git")).is_none());
    }
}
