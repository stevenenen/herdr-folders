//! Herdr Folders: walk a folder tree, recent folders pinned to the top, open a tab or worktree.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use ratatui::{
    DefaultTerminal, Frame,
    crossterm::event::{self, Event, KeyCode, KeyEventKind},
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

fn herdr_bin() -> String {
    std::env::var("HERDR_BIN_PATH")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "herdr".into())
}

fn herdr(args: &[&str]) -> Option<Value> {
    let out = Command::new(herdr_bin()).args(args).output().ok()?;
    let value: Value = serde_json::from_slice(&out.stdout).ok()?;
    value.get("result").cloned()
}

/// Herdr's JSON result, or its error message.
fn herdr_result(args: &[&str]) -> Result<Value, String> {
    let out = Command::new(herdr_bin())
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    let value: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    if out.status.success()
        && let Some(result) = value.get("result")
    {
        return Ok(result.clone());
    }
    Err(value["error"]["message"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| String::from_utf8_lossy(&out.stderr).trim().to_string()))
}

/// Runs git in `dir`. Credential prompts are off: they would draw over the popup.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

fn expand_home(s: &str) -> PathBuf {
    match s.strip_prefix("~/") {
        Some(rest) => Path::new(&home()).join(rest),
        None => PathBuf::from(s),
    }
}

/// `~`-relative display of a path under `$HOME`, else the path unchanged.
fn display_path(p: &Path) -> String {
    let home = home();
    p.strip_prefix(&home)
        .map(|rest| format!("~/{}", rest.display()))
        .unwrap_or_else(|_| p.display().to_string())
}

fn config_path() -> Option<PathBuf> {
    std::env::var("HERDR_PLUGIN_CONFIG_DIR")
        .ok()
        .map(|d| PathBuf::from(d).join("config.toml"))
}

/// `[dirs] roots = [...]` from the plugin's config.toml, with `~` expanded. Only folders that
/// exist survive, so the same config works across Macs where the vault lives in a different
/// iCloud container.
fn config_roots(config: &str) -> Vec<PathBuf> {
    config
        .parse::<toml::Table>()
        .ok()
        .and_then(|t| t.get("dirs")?.get("roots")?.as_array().cloned())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(expand_home))
        .filter(|p| p.is_dir())
        .collect()
}

/// Most recently opened first, from `$HERDR_PLUGIN_STATE_DIR/recent.json`. Keyed by absolute
/// path, kept small so the file never grows unbounded.
#[derive(Default)]
struct Recent(HashMap<String, u64>);

const RECENT_CAP: usize = 200;
const RECENT_SHOWN: usize = 12;

impl Recent {
    fn path() -> Option<PathBuf> {
        std::env::var("HERDR_PLUGIN_STATE_DIR")
            .ok()
            .map(|d| PathBuf::from(d).join("recent.json"))
    }

    fn load() -> Self {
        Self::path()
            .and_then(|p| fs::read(p).ok())
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .map(|v| {
                Recent(
                    v.as_object()
                        .into_iter()
                        .flatten()
                        .filter_map(|(k, v)| Some((k.clone(), v.as_u64()?)))
                        .collect(),
                )
            })
            .unwrap_or_default()
    }

    fn save(&self) {
        if let Some(p) = Self::path() {
            let _ = fs::write(p, serde_json::to_string(&self.0).unwrap_or_default());
        }
    }

    fn touch(&mut self, dir: &Path) {
        let key = fs::canonicalize(dir)
            .unwrap_or_else(|_| dir.to_path_buf())
            .to_string_lossy()
            .into_owned();
        self.0.insert(key, now_millis());
        if self.0.len() > RECENT_CAP {
            let mut by_age: Vec<(String, u64)> =
                self.0.iter().map(|(k, v)| (k.clone(), *v)).collect();
            by_age.sort_by_key(|(_, ts)| *ts);
            for (k, _) in by_age.into_iter().take(self.0.len() - RECENT_CAP) {
                self.0.remove(&k);
            }
        }
        self.save();
    }

    /// Existing paths only, most recent first, capped to `RECENT_SHOWN`.
    fn recent_paths(&self) -> Vec<PathBuf> {
        let mut list: Vec<(&String, &u64)> = self.0.iter().collect();
        list.sort_by_key(|(_, ts)| std::cmp::Reverse(**ts));
        list.into_iter()
            .map(|(p, _)| PathBuf::from(p))
            .filter(|p| p.is_dir())
            .take(RECENT_SHOWN)
            .collect()
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Direct subfolders of `dir`, dotfolders skipped, by name (case-insensitive).
fn children_of(dir: &Path) -> Vec<PathBuf> {
    let mut list: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    list.sort_by_key(|p| p.file_name().map(|n| n.to_ascii_lowercase()));
    list
}

fn is_repo_root(dir: &Path) -> bool {
    dir.join(".git").is_dir()
}

/// The repo a folder lives in, main clone or linked worktree, by its common git dir.
fn repo_root(dir: &Path) -> Option<PathBuf> {
    let common = git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    Some(Path::new(&common).parent().unwrap_or(dir).to_path_buf())
}

/// `prefix` plus the first words of `title`, lowercase and hyphenated, within 48 chars.
fn slug(prefix: &str, title: &str) -> String {
    let mut name = prefix.to_string();
    let lower = title.to_lowercase();
    for word in lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        if name.len() + word.len() + 1 > 48 {
            break;
        }
        name = format!("{name}-{word}");
    }
    name
}

fn branch_slug(text: &str) -> String {
    slug("", text).trim_start_matches('-').to_string()
}

/// Branch name for opening `dir` as a worktree of `repo`: the repo root itself gets `work`, a
/// subfolder gets its path relative to the repo, slugged. `free_branch` numbers it when taken.
fn branch_for(repo: &Path, dir: &Path) -> String {
    let rel = dir.strip_prefix(repo).unwrap_or(Path::new(""));
    if rel.as_os_str().is_empty() {
        "work".to_string()
    } else {
        let name = branch_slug(&rel.to_string_lossy());
        if name.is_empty() {
            "work".to_string()
        } else {
            name
        }
    }
}

/// `name`, or else the first of `name-2`, `name-3`, … that is not `taken`.
fn first_free(name: &str, taken: impl Fn(&str) -> bool) -> String {
    std::iter::once(name.to_string())
        .chain((2..).map(|n| format!("{name}-{n}")))
        .find(|b| !taken(b))
        .unwrap_or_default()
}

/// A branch name for a new worktree of `clone`: neither the branch nor its folder exists yet.
fn free_branch(clone: &Path, name: &str) -> String {
    first_free(name, |b| {
        worktree_path(clone, b).exists()
            || git(
                clone,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{b}"),
                ],
            )
            .is_some()
    })
}

/// The remote's default branch as last fetched: `origin/main`. No network, so it is instant.
fn remote_head(clone: &Path) -> String {
    git(
        clone,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .unwrap_or_else(|| "origin/main".into())
}

/// The remote's default branch, freshly fetched: `origin/main`.
fn default_base(clone: &Path) -> String {
    let base = remote_head(clone);
    git(
        clone,
        &["fetch", "origin", base.trim_start_matches("origin/")],
    );
    base
}

/// The open workspace of the clone itself. Herdr only starts worktrees from the repo's own
/// workspace, and resolving `--cwd` can land on a linked worktree of it instead.
fn source_workspace(clone: &Path) -> Option<String> {
    let clone = fs::canonicalize(clone).ok()?;
    herdr(&["workspace", "list"])?["workspaces"]
        .as_array()?
        .iter()
        .find(|w| {
            let wt = &w["worktree"];
            wt["is_linked_worktree"] == false
                && wt["checkout_path"]
                    .as_str()
                    .and_then(|p| fs::canonicalize(p).ok())
                    .as_ref()
                    == Some(&clone)
        })
        .and_then(|w| w["workspace_id"].as_str().map(str::to_string))
}

/// Where Herdr puts a new worktree: `<worktrees.directory>/<repo folder>/<branch slug>`.
fn worktree_path(clone: &Path, branch: &str) -> PathBuf {
    let configured = std::env::var("HERDR_CONFIG_PATH")
        .ok()
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&home()).join(".config/herdr/config.toml"));
    let dir = fs::read_to_string(configured)
        .ok()
        .and_then(|c| c.parse::<toml::Table>().ok())
        .and_then(|t| Some(t.get("worktrees")?.get("directory")?.as_str()?.to_string()))
        .unwrap_or_else(|| "~/.herdr/worktrees".into());
    let dir = expand_home(&dir);
    let repo = clone.file_name().unwrap_or_default();
    dir.join(repo).join(branch.replace('/', "-"))
}

/// Opens `dir` as a plain tab, named after it. For anything outside a git repo, or when a
/// worktree was explicitly declined.
fn open_as_tab(dir: &Path) -> Result<(), String> {
    let cwd = dir.to_string_lossy();
    let name = dir.file_name().unwrap_or_default().to_string_lossy();
    herdr_result(&["tab", "create", "--cwd", &cwd, "--label", &name, "--focus"]).map(|_| ())
}

/// `pane run` types into the shell, so a free-text prompt goes in single quotes.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Opens (or reopens) a worktree of `repo` on `branch`, Claude started in it (on `prompt` when
/// not empty). `label` names the workspace, `tab_label` the tab.
fn open_worktree_full(
    repo: &Path,
    branch: &str,
    base: Option<&str>,
    label: &str,
    tab_label: &str,
    prompt: &str,
) -> Result<(), String> {
    let cwd = repo.to_string_lossy().to_string();
    let (flag, value) = match source_workspace(repo) {
        Some(id) => ("--workspace", id),
        None => ("--cwd", cwd),
    };
    let source = [flag, value.as_str()];

    let path = worktree_path(repo, branch);
    if path.exists() {
        let path = path.to_string_lossy();
        let reopen = [
            "worktree", "open", source[0], source[1], "--path", &path, "--label", label, "--focus",
        ];
        return herdr_result(&reopen).map(|_| ());
    }

    let mut args = vec![
        "worktree", "create", source[0], source[1], "--branch", branch, "--label", label, "--focus",
    ];
    if let Some(base) = base {
        args.extend(["--base", base]);
    }
    let created = herdr_result(&args)?;
    if let Some(tab) = created["tab"]["tab_id"].as_str() {
        herdr(&["tab", "rename", tab, tab_label]);
    }
    if let Some(pane) = created["root_pane"]["pane_id"].as_str() {
        let cmd = match prompt {
            "" => "claude".to_string(),
            p => format!("claude {}", shell_quote(p)),
        };
        let _ = Command::new(herdr_bin())
            .args(["pane", "run", pane, &cmd])
            .output();
    }
    Ok(())
}

/// Creates a new worktree of `repo` for `dir`, branch and workspace named by `branch_for` (numbered
/// when taken), Claude started in it. Never reopens an existing one, and never waits on the
/// network: it starts from the default branch as last fetched.
fn open_as_worktree(repo: &Path, dir: &Path) -> Result<(), String> {
    let branch = free_branch(repo, &branch_for(repo, dir));
    let tab_label: String = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| branch.clone());
    let base = remote_head(repo);
    open_worktree_full(repo, &branch, Some(&base), &branch, &tab_label, "")
}

/// Opens `dir`: a worktree when it is inside a git repo and `force_tab` is false, else a plain
/// tab there.
fn open_folder(dir: &Path, force_tab: bool) -> Result<(), String> {
    match (!force_tab).then(|| repo_root(dir)).flatten() {
        Some(repo) => open_as_worktree(&repo, dir),
        None => open_as_tab(dir),
    }
}

/// An issue or pull request link: `https://github.com/<owner>/<repo>/(issues|pull)/<n>...`.
#[derive(Debug, PartialEq)]
struct GithubLink {
    repo: String,
    pull: bool,
    number: u64,
}

fn parse_link(url: &str) -> Option<GithubLink> {
    let url = url.trim();
    let url = url.split_once("://").map_or(url, |(_, rest)| rest);
    let url = url.strip_prefix("www.").unwrap_or(url);
    let mut parts = url.strip_prefix("github.com/")?.split('/');
    let (owner, repo, kind, number) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    let digits: String = number.chars().take_while(char::is_ascii_digit).collect();
    Some(GithubLink {
        repo: format!("{owner}/{repo}").to_lowercase(),
        pull: match kind {
            "issues" => false,
            "pull" => true,
            _ => return None,
        },
        number: digits.parse().ok()?,
    })
}

/// `owner/repo` from a GitHub remote URL, over HTTPS or SSH (any `github.com*` host alias).
fn remote_repo(remote: &str) -> Option<String> {
    let remote = remote.trim();
    if !remote.contains("github.com") {
        return None;
    }
    let path = match remote.split_once("://") {
        Some((_, rest)) => rest.split_once('/')?.1,
        None => remote.split_once(':')?.1,
    };
    Some(
        path.trim_end_matches('/')
            .trim_end_matches(".git")
            .to_lowercase(),
    )
}

/// The main clone of `repo` among the folders directly inside `dirs`.
fn find_clone(dirs: &[PathBuf], repo: &str) -> Option<PathBuf> {
    dirs.iter()
        .filter_map(|d| fs::read_dir(d).ok())
        .flatten()
        .flatten()
        .map(|e| e.path())
        // A main clone has a `.git` folder; a linked worktree only has a `.git` file.
        .filter(|p| p.join(".git").is_dir())
        .find(|p| {
            git(p, &["remote", "get-url", "origin"])
                .and_then(|url| remote_repo(&url))
                .as_deref()
                == Some(repo)
        })
}

/// Clones `repo` into `<dir>/<repo name>` with `gh`, for a link to a repo not cloned yet.
fn clone_repo(dir: &Path, repo: &str) -> Result<PathBuf, String> {
    let target = dir.join(repo.rsplit('/').next().unwrap_or(repo));
    if target.exists() {
        return Err(format!(
            "{} exists but is not a clone of {repo}",
            target.display()
        ));
    }
    let out = Command::new("gh")
        .args(["repo", "clone", repo])
        .arg(&target)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| format!("gh: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "gh repo clone: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(target)
}

/// `issue-874-prove-a-customer-can-only-see`: the number plus the title's first words.
fn branch_name(number: u64, title: &str) -> String {
    slug(&format!("issue-{number}"), title)
}

/// Opens the worktree for a GitHub issue or pull request link, with Claude started on it.
/// Pasting the same link again reopens that worktree instead of creating another.
fn open_github_link(url: &str, roots: &[PathBuf]) -> Result<(), String> {
    let link = parse_link(url).ok_or("not a GitHub issue or pull request link")?;
    if roots.is_empty() {
        return Err(format!(
            "set [dirs] roots in {}",
            config_path()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        ));
    }
    let clone = match find_clone(roots, &link.repo) {
        Some(clone) => clone,
        None => clone_repo(&roots[0], &link.repo)?,
    };
    let (kind, fields) = if link.pull {
        ("pr", "title,headRefName")
    } else {
        ("issue", "title")
    };
    let number = link.number.to_string();
    let out = Command::new("gh")
        .args([kind, "view", &number, "-R", &link.repo, "--json", fields])
        .output()
        .map_err(|e| format!("gh: {e}"))?;
    let info: Value = serde_json::from_slice(&out.stdout)
        .map_err(|_| format!("gh: {}", String::from_utf8_lossy(&out.stderr).trim()))?;
    let title = info["title"].as_str().unwrap_or_default();
    let branch = match info["headRefName"].as_str() {
        Some(head) => head.to_string(),
        None => branch_name(link.number, title),
    };
    let label = format!("#{} {title}", link.number);
    let base = if link.pull {
        git(
            &clone,
            &["fetch", "origin", &format!("pull/{number}/head:{branch}")],
        );
        None
    } else {
        Some(default_base(&clone))
    };
    open_worktree_full(&clone, &branch, base.as_deref(), &label, &label, url.trim())
}

/// A pasted absolute path, `~` expanded: the folder itself, or a file's containing folder.
fn resolve_path(raw: &str) -> Result<PathBuf, String> {
    let path = expand_home(raw.trim());
    if path.is_dir() {
        Ok(path)
    } else if path.is_file() {
        path.parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| format!("no parent folder: {}", path.display()))
    } else {
        Err(format!("not a file or folder: {}", path.display()))
    }
}

/// Case-insensitive: every word of the filter must appear somewhere in the fields.
fn matches(filter: &str, fields: &[&str]) -> bool {
    let haystack = fields.join(" ").to_lowercase();
    filter
        .to_lowercase()
        .split_whitespace()
        .all(|word| haystack.contains(word))
}

enum Row {
    Header(String),
    Entry(PathBuf),
}

struct App {
    roots: Vec<PathBuf>,
    /// The folders entered so far; empty means the virtual root (Recent + Browse).
    stack: Vec<PathBuf>,
    recent: Recent,
    filter: String,
    typing: bool,
    list: ListState,
    notice: Option<String>,
    configured: bool,
    /// A GitHub link or absolute path being typed after `u`.
    link: Option<String>,
}

impl App {
    fn load() -> Self {
        let config = config_path()
            .and_then(|p| fs::read_to_string(p).ok())
            .unwrap_or_default();
        let roots = config_roots(&config);
        App {
            configured: !roots.is_empty(),
            roots,
            stack: Vec::new(),
            recent: Recent::load(),
            filter: String::new(),
            typing: false,
            list: ListState::default().with_selected(Some(0)),
            notice: None,
            link: None,
        }
    }

    fn breadcrumb(&self) -> String {
        self.stack
            .last()
            .map(|p| display_path(p))
            .unwrap_or_default()
    }

    /// Rows to draw, in order: a header per non-empty group, its entries, filter already applied.
    fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        if let Some(dir) = self.stack.last() {
            let kids: Vec<PathBuf> = children_of(dir)
                .into_iter()
                .filter(|p| matches(&self.filter, &[&p.file_name_str()]))
                .collect();
            rows.push(Row::Header(self.breadcrumb()));
            rows.extend(kids.into_iter().map(Row::Entry));
        } else {
            let recent: Vec<PathBuf> = self
                .recent
                .recent_paths()
                .into_iter()
                .filter(|p| matches(&self.filter, &[&p.to_string_lossy()]))
                .collect();
            if !recent.is_empty() {
                rows.push(Row::Header("Recent".into()));
                rows.extend(recent.into_iter().map(Row::Entry));
            }
            let roots: Vec<PathBuf> = self
                .roots
                .iter()
                .filter(|p| matches(&self.filter, &[&p.to_string_lossy()]))
                .cloned()
                .collect();
            if !roots.is_empty() {
                rows.push(Row::Header("Browse".into()));
                rows.extend(roots.into_iter().map(Row::Entry));
            }
        }
        rows
    }

    fn entries(&self) -> Vec<PathBuf> {
        self.rows()
            .into_iter()
            .filter_map(|r| match r {
                Row::Entry(p) => Some(p),
                Row::Header(_) => None,
            })
            .collect()
    }

    fn selected(&self) -> Option<PathBuf> {
        let i = self.list.selected()?;
        self.entries().get(i).cloned()
    }

    fn step(&mut self, delta: isize) {
        let len = self.entries().len();
        if len > 0 {
            let i = self.list.selected().unwrap_or(0) as isize + delta;
            self.list
                .select(Some(i.clamp(0, len as isize - 1) as usize));
        }
    }

    fn reset_selection(&mut self) {
        self.filter.clear();
        self.list.select(Some(0));
        self.notice = None;
    }

    fn descend(&mut self) {
        if let Some(dir) = self.selected() {
            self.stack.push(dir);
            self.reset_selection();
        }
    }

    /// Pops one level, reselecting the folder just left. Returns false when already at the
    /// virtual root.
    fn ascend(&mut self) -> bool {
        if let Some(left) = self.stack.pop() {
            self.reset_selection();
            let i = self.entries().iter().position(|p| *p == left);
            self.list.select(Some(i.unwrap_or(0)));
            true
        } else {
            false
        }
    }

    /// Esc steps back one layer: stop typing, then clear the filter, then go up a folder, then
    /// close. Returns true when the popup should close.
    fn back_out(&mut self) -> bool {
        if self.typing {
            self.typing = false;
        } else if !self.filter.is_empty() {
            self.filter.clear();
            self.list.select(Some(0));
        } else if !self.ascend() {
            return true;
        }
        false
    }

    /// Opens the selected folder, recording it as recently used on success.
    fn open(&mut self, force_tab: bool) -> Option<Result<(), String>> {
        let dir = self.selected()?;
        let result = open_folder(&dir, force_tab);
        if result.is_ok() {
            self.recent.touch(&dir);
        }
        Some(result)
    }

    /// Resolves a pasted GitHub issue/pull-request link or an absolute path, and opens it: a
    /// link opens (or reopens) its worktree; a path opens the same way `Enter` would.
    fn open_link(&mut self, raw: &str) -> Result<(), String> {
        if parse_link(raw).is_some() {
            return open_github_link(raw, &self.roots);
        }
        let dir = resolve_path(raw)?;
        let result = open_folder(&dir, false);
        if result.is_ok() {
            self.recent.touch(&dir);
        }
        result
    }
}

trait FileNameStr {
    fn file_name_str(&self) -> String;
}

impl FileNameStr for PathBuf {
    fn file_name_str(&self) -> String {
        self.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

fn main() -> std::io::Result<()> {
    let mut terminal = ratatui::init();
    let result = run(&mut terminal);
    ratatui::restore();
    result
}

fn run(terminal: &mut DefaultTerminal) -> std::io::Result<()> {
    let mut app = App::load();
    loop {
        terminal.draw(|f| draw(f, &mut app))?;
        if !event::poll(Duration::from_millis(500))? {
            continue;
        }
        // A paste delivers many key events in one burst; apply all of them before redrawing,
        // so it lands instantly instead of visibly typing one character per frame.
        loop {
            if let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                if let Some(text) = app.link.as_mut() {
                    match key.code {
                        KeyCode::Char(c) => text.push(c),
                        KeyCode::Backspace => {
                            text.pop();
                        }
                        KeyCode::Esc => app.link = None,
                        KeyCode::Enter => {
                            let raw = app.link.take().unwrap_or_default();
                            app.notice = Some(format!("opening {}…", raw.trim()));
                            terminal.draw(|f| draw(f, &mut app))?;
                            match app.open_link(&raw) {
                                Ok(()) => return Ok(()),
                                Err(e) => app.notice = Some(e),
                            }
                        }
                        _ => {}
                    }
                } else if app.typing
                    && let KeyCode::Char(c) = key.code
                {
                    app.filter.push(c);
                    app.list.select(Some(0));
                } else if app.typing && key.code == KeyCode::Backspace {
                    app.filter.pop();
                    app.list.select(Some(0));
                } else {
                    match key.code {
                        KeyCode::Esc => {
                            if app.back_out() {
                                return Ok(());
                            }
                        }
                        KeyCode::Char('/') => app.typing = true,
                        KeyCode::Char('u') => {
                            app.link = Some(String::new());
                            app.notice = None;
                        }
                        KeyCode::Char('j') | KeyCode::Down => app.step(1),
                        KeyCode::Char('k') | KeyCode::Up => app.step(-1),
                        KeyCode::Char('g') | KeyCode::Home => app.step(isize::MIN / 2),
                        KeyCode::Char('G') | KeyCode::End => app.step(isize::MAX / 2),
                        KeyCode::Backspace => {
                            app.ascend();
                        }
                        KeyCode::Tab => app.descend(),
                        KeyCode::Enter | KeyCode::BackTab if app.selected().is_some() => {
                            let force_tab = key.code == KeyCode::BackTab;
                            app.notice = Some(if force_tab {
                                "opening tab…".into()
                            } else {
                                "creating worktree…".into()
                            });
                            terminal.draw(|f| draw(f, &mut app))?;
                            match app.open(force_tab) {
                                Some(Err(e)) => app.notice = Some(e),
                                _ => return Ok(()),
                            }
                        }
                        _ => {}
                    }
                }
            }
            if !event::poll(Duration::ZERO)? {
                break;
            }
        }
    }
}

/// Tokyo Night, the same dark palette the other Herdr plugins use.
mod palette {
    use ratatui::style::Color;
    pub const BG: Color = Color::Rgb(26, 27, 38);
    pub const SELECTED: Color = Color::Rgb(65, 72, 104);
    pub const TEXT: Color = Color::Rgb(192, 202, 245);
    pub const SUBTEXT: Color = Color::Rgb(169, 177, 214);
    pub const ACCENT: Color = Color::Rgb(122, 162, 247);
    pub const RED: Color = Color::Rgb(247, 118, 142);
    pub const MAUVE: Color = Color::Rgb(187, 154, 247);
}

/// Truncates to `width` columns with an ellipsis (keeping the start), then pads to exactly
/// `width`.
fn fit(text: &str, width: usize) -> String {
    if text.width() <= width {
        return format!(
            "{text:<width$}",
            width = width - text.width() + text.chars().count()
        );
    }
    let mut out = String::new();
    for c in text.chars() {
        if out.width() + c.width().unwrap_or(0) + 1 > width {
            break;
        }
        out.push(c);
    }
    out.push('…');
    let pad = width.saturating_sub(out.width());
    out + &" ".repeat(pad)
}

/// Truncates to `width` columns with a leading ellipsis (keeping the end) — for paths, where
/// the meaningful part is the tail.
fn fit_tail(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let keep = width.saturating_sub(1);
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<char> = Vec::new();
    let mut w = 0usize;
    for c in chars.iter().rev() {
        let cw = c.width().unwrap_or(0);
        if w + cw > keep {
            break;
        }
        w += cw;
        out.push(*c);
    }
    out.reverse();
    format!("…{}", out.into_iter().collect::<String>())
}

fn header(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!(" {text} "),
        Style::new().fg(palette::BG).bg(palette::ACCENT).bold(),
    ))
}

fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    f.render_widget(
        Block::new().style(Style::new().bg(palette::BG).fg(palette::TEXT)),
        area,
    );

    if !app.configured {
        let path = config_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        f.render_widget(
            Paragraph::new(format!(
                "No folders configured yet.\n\nAdd [dirs] roots = [\"~/Development\"] to:\n{path}"
            ))
            .style(Style::new().fg(palette::SUBTEXT)),
            area,
        );
        return;
    }

    let bar = app.notice.is_some();
    let [bar_area, body, footer] = Layout::vertical([
        Constraint::Length(if bar { 1 } else { 0 }),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(area);

    if let Some(notice) = &app.notice {
        f.render_widget(
            Paragraph::new(notice.as_str()).style(Style::new().fg(palette::RED)),
            bar_area,
        );
    }

    let filter_line = Line::from(vec![
        Span::styled("  / ", Style::new().fg(palette::SUBTEXT)),
        Span::styled(
            if app.filter.is_empty() && !app.typing {
                "to filter".to_string()
            } else {
                app.filter.clone()
            },
            Style::new().fg(palette::TEXT),
        ),
        Span::styled(
            if app.typing { "▏" } else { "" },
            Style::new().fg(palette::ACCENT),
        ),
    ]);

    let [filter_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(body);
    f.render_widget(Paragraph::new(filter_line), filter_area);

    let rows = app.rows();
    let width = list_area.width.saturating_sub(4) as usize;
    let items: Vec<ListItem> = rows
        .iter()
        .map(|r| match r {
            Row::Header(text) => ListItem::new(header(&fit_tail(text, width))),
            Row::Entry(p) => {
                let repo = is_repo_root(p);
                let label = if app.stack.is_empty() {
                    display_path(p)
                } else {
                    p.file_name_str()
                };
                let icon = if repo { "⎇ " } else { "  " };
                ListItem::new(Line::from(vec![
                    Span::styled(icon, Style::new().fg(palette::MAUVE)),
                    Span::styled(
                        fit_tail(&label, width.saturating_sub(2)),
                        Style::new().fg(palette::TEXT),
                    ),
                ]))
            }
        })
        .collect();

    // Map the selected entry index to its position among the mixed header/entry rows.
    let mut entry_seen = 0usize;
    let mut list_selected = None;
    for (i, r) in rows.iter().enumerate() {
        if let Row::Entry(_) = r {
            if Some(entry_seen) == app.list.selected() {
                list_selected = Some(i);
                break;
            }
            entry_seen += 1;
        }
    }
    let mut list_state = ListState::default().with_selected(list_selected);

    f.render_stateful_widget(
        List::new(items)
            .block(Block::new().borders(Borders::NONE))
            .highlight_style(Style::new().bg(palette::SELECTED).fg(palette::TEXT))
            .style(Style::new().fg(palette::SUBTEXT)),
        list_area,
        &mut list_state,
    );

    let hint = if app.typing {
        "Esc stop typing"
    } else {
        "j/k move  Tab go into folder  Backspace up  Enter open (worktree if git)  Shift+Tab plain tab  u paste link/path  / filter  Esc back/close"
    };
    f.render_widget(
        Paragraph::new(fit(hint, footer.width as usize)).style(Style::new().fg(palette::SUBTEXT)),
        footer,
    );

    if let Some(text) = &app.link {
        let popup = centered_rect(area, 70, 5);
        f.render_widget(Clear, popup);
        let block = Block::new()
            .borders(Borders::ALL)
            .border_style(Style::new().fg(palette::ACCENT))
            .title(Span::styled(
                " Link or path ",
                Style::new().fg(palette::ACCENT).bold(),
            ))
            .style(Style::new().bg(palette::BG).fg(palette::TEXT));
        let inner = block.inner(popup);
        f.render_widget(block, popup);
        let shown = fit_tail(text, inner.width.saturating_sub(1) as usize);
        let lines = vec![
            Line::from(vec![
                Span::raw(shown),
                Span::styled("▏", Style::new().fg(palette::ACCENT)),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Enter open   Esc cancel",
                Style::new().fg(palette::SUBTEXT),
            )),
        ];
        f.render_widget(Paragraph::new(lines), inner);
    }
}

/// A `width_pct`-wide, `height`-tall box centered in `area`.
fn centered_rect(area: Rect, width_pct: u16, height: u16) -> Rect {
    let width = area.width * width_pct / 100;
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_issue_and_pr_links() {
        let issue = parse_link("https://github.com/acme/widgets/issues/874").unwrap();
        assert_eq!(issue.repo, "acme/widgets");
        assert!(!issue.pull);
        assert_eq!(issue.number, 874);

        let pr = parse_link("github.com/acme/widgets/pull/12?x=1").unwrap();
        assert!(pr.pull);
        assert_eq!(pr.number, 12);

        assert!(parse_link("https://github.com/acme/widgets").is_none());
        assert!(parse_link("not a link").is_none());
    }

    #[test]
    fn remote_repo_handles_https_and_ssh() {
        assert_eq!(
            remote_repo("https://github.com/acme/widgets.git"),
            Some("acme/widgets".into())
        );
        assert_eq!(
            remote_repo("git@github.com:acme/widgets.git"),
            Some("acme/widgets".into())
        );
        assert_eq!(remote_repo("https://gitlab.com/acme/widgets.git"), None);
    }

    #[test]
    fn branch_name_slugs_the_title() {
        assert_eq!(
            branch_name(874, "Prove a customer can only see"),
            "issue-874-prove-a-customer-can-only-see"
        );
    }

    #[test]
    fn branch_for_repo_root_is_work() {
        let repo = Path::new("/tmp/acme");
        assert_eq!(branch_for(repo, repo), "work");
        assert_eq!(
            branch_for(repo, &repo.join("docs").join("setup")),
            "docs-setup"
        );
    }

    #[test]
    fn first_free_numbers_taken_names() {
        assert_eq!(first_free("work", |_| false), "work");
        assert_eq!(
            first_free("work", |b| b == "work" || b == "work-2"),
            "work-3"
        );
    }

    #[test]
    fn resolve_path_expands_home_and_finds_parent() {
        let dir = std::env::temp_dir().join(format!("herdr-folders-test-{}", now_millis()));
        let file = dir.join("note.md");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&file, "x").unwrap();

        assert_eq!(resolve_path(dir.to_str().unwrap()).unwrap(), dir);
        assert_eq!(resolve_path(file.to_str().unwrap()).unwrap(), dir);
        assert!(resolve_path(&dir.join("missing").to_string_lossy()).is_err());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn matches_requires_every_word() {
        assert!(matches("ac wi", &["acme widgets"]));
        assert!(!matches("ac zz", &["acme widgets"]));
        assert!(matches("", &["anything"]));
    }
}
