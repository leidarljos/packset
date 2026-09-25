//! What the checkout says, as opposed to what the pack remembers.
//!
//! Rules, skills and the file outline are read from the working tree on every
//! call and never written back. That separation is the point: the pack holds
//! what somebody chose to remember, and this holds what is simply true of the
//! directory right now, so neither can quietly become the other.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

/// Rule files a project writes for itself.
pub const PROJECT_RULE_NAMES: &[&str] = &["AGENTS.md", "WARP.md"];
/// Rule files other tools read, which a project may also carry.
pub const LINKED_RULE_NAMES: &[&str] = &[
    "CLAUDE.md",
    "AGENT.md",
    "GEMINI.md",
    ".cursorrules",
    ".clinerules",
    ".windsurfrules",
];
/// Linked rules that live at a fixed path under the root.
pub const LINKED_RULE_RELPATHS: &[&str] = &[".github/copilot-instructions.md"];

/// Directories a skill catalog may live in.
pub const SKILL_DIR_NAMES: &[&str] = &[
    ".agents/skills",
    ".warp/skills",
    ".claude/skills",
    ".codex/skills",
    ".cursor/skills",
    ".gemini/skills",
    ".copilot/skills",
    ".factory/skills",
    ".github/skills",
    ".opencode/skills",
    ".grok/skills",
];

/// Ignore files whose patterns narrow the outline.
pub const IGNORE_FILE_NAMES: &[&str] = &[
    ".warpindexingignore",
    ".cursorignore",
    ".cursorindexingignore",
    ".codeiumignore",
    ".grokindexingignore",
];

/// Past this many files the outline lists none of them.
pub const MAP_TOO_LARGE: usize = 5000;
/// The most paths one outline names.
pub const MAP_LIST_CAP: usize = 500;
/// The most characters one attached body carries.
pub const ATTACH_CAP: usize = 32 * 1024;

fn read_text(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn git_line(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// The repository toplevel, or nothing when this is not a work tree.
#[must_use]
pub fn git_root(cwd: &Path) -> Option<PathBuf> {
    git_line(cwd, &["rev-parse", "--show-toplevel"]).map(PathBuf::from)
}

/// The resolved git directory, which a worktree does not share.
#[must_use]
pub fn git_dir(cwd: &Path) -> Option<PathBuf> {
    let text = git_line(cwd, &["rev-parse", "--git-dir"])?;
    let path = PathBuf::from(text);
    if path.is_absolute() {
        Some(path)
    } else {
        std::fs::canonicalize(cwd.join(path)).ok()
    }
}

fn resolve(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// `cwd` first, then its parents, ending at `stop`.
#[must_use]
pub fn walk_to_root(cwd: &Path, stop: &Path) -> Vec<PathBuf> {
    let mut current = resolve(cwd);
    let stop = resolve(stop);
    let mut out = Vec::new();
    loop {
        out.push(current.clone());
        if current == stop || current.parent().is_none_or(|p| p == current) {
            break;
        }
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => break,
        }
    }
    out
}

/// A leading `--- key: value ---` block and the body after it.
///
/// Not YAML: one level of `key: value`, quotes stripped, and a file without the
/// block is all body. A skill that meant to declare a name and did not is read
/// as prose rather than half-parsed.
#[must_use]
pub fn parse_frontmatter(text: &str) -> (BTreeMap<String, String>, String) {
    let Some(rest) = text.strip_prefix("---") else {
        return (BTreeMap::new(), text.to_string());
    };
    let rest = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))
        .unwrap_or(rest);
    let Some(end) = rest.find("\n---") else {
        return (BTreeMap::new(), text.to_string());
    };
    let block = &rest[..end];
    let body = &rest[end + 4..];
    let body = body
        .strip_prefix("\r\n")
        .or_else(|| body.strip_prefix('\n'))
        .unwrap_or(body);
    let mut meta = BTreeMap::new();
    for line in block.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let name = key.trim().to_ascii_lowercase();
        if name.is_empty() {
            continue;
        }
        let value = value.trim().trim_matches('"').trim_matches('\'');
        meta.insert(name, value.to_string());
    }
    (meta, body.to_string())
}

/// Rule files, most specific first.
///
/// Bodies stay on disk unless the caller asks: a listing is cheap and a client
/// that only wants to know what exists should not pay for reading it.
#[must_use]
pub fn discover_rules(cwd: &Path, user_card: &Path) -> Vec<Value> {
    let here = resolve(cwd);
    let root = git_root(&here).unwrap_or_else(|| here.clone());
    let mut found = Vec::new();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();

    let mut add = |path: PathBuf, kind: &str, scope: &str, found: &mut Vec<Value>| {
        if !path.is_file() {
            return;
        }
        let resolved = resolve(&path);
        if !seen.insert(resolved.clone()) {
            return;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        let relpath = resolved
            .strip_prefix(resolve(&root))
            .map_or_else(|_| name.clone(), |p| p.display().to_string());
        found.push(json!({
            "path": resolved.display().to_string(),
            "relpath": relpath,
            "kind": kind,
            "scope": scope,
            "name": name,
        }));
    };

    for directory in walk_to_root(&here, &root) {
        let scope = if directory == here {
            "cwd"
        } else if directory == resolve(&root) {
            "root"
        } else {
            "parent"
        };
        for name in PROJECT_RULE_NAMES {
            add(directory.join(name), "project", scope, &mut found);
        }
        for name in LINKED_RULE_NAMES {
            add(directory.join(name), "linked", scope, &mut found);
        }
        if directory == resolve(&root) {
            for rel in LINKED_RULE_RELPATHS {
                add(directory.join(rel), "linked", "root", &mut found);
            }
        }
    }

    if user_card.is_file() && !read_text(user_card).trim().is_empty() {
        found.push(json!({
            "path": resolve(user_card).display().to_string(),
            "relpath": "USER.md",
            "kind": "global",
            "scope": "global",
            "name": "USER.md",
        }));
    }
    found
}

fn skill_dirs(cwd: &Path, root: &Path, home: &Path) -> Vec<(PathBuf, &'static str)> {
    let mut pairs = Vec::new();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    for directory in walk_to_root(cwd, root) {
        for rel in SKILL_DIR_NAMES {
            let candidate = directory.join(rel);
            if !candidate.is_dir() {
                continue;
            }
            if seen.insert(resolve(&candidate)) {
                pairs.push((candidate, "project"));
            }
        }
    }
    for rel in SKILL_DIR_NAMES {
        let candidate = home.join(rel);
        if !candidate.is_dir() {
            continue;
        }
        if seen.insert(resolve(&candidate)) {
            pairs.push((candidate, "global"));
        }
    }
    pairs
}

fn skill_from_dir(dir: &Path, scope: &str) -> Option<Value> {
    let path = dir.join("SKILL.md");
    if !path.is_file() {
        return None;
    }
    let text = read_text(&path);
    if text.trim().is_empty() {
        return None;
    }
    let (meta, _body) = parse_frontmatter(&text);
    let dir_name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    let name = meta
        .get("name")
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| dir_name.clone());
    // A skill with no declared description falls back to its first heading,
    // which is what a reader would have skimmed anyway.
    let description = meta
        .get("description")
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
        .or_else(|| {
            text.lines()
                .map(str::trim)
                .find(|line| {
                    line.starts_with('#') && !line.trim_start_matches('#').trim().is_empty()
                })
                .map(|line| line.trim_start_matches('#').trim().to_string())
        })
        .unwrap_or_default();
    let mut supporting: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| n != "SKILL.md")
        .collect();
    supporting.sort();
    Some(json!({
        "name": name,
        "description": description,
        "path": resolve(&path).display().to_string(),
        "dir": resolve(dir).display().to_string(),
        "scope": scope,
        "supporting": supporting,
    }))
}

/// The skill catalog: names, descriptions and paths, with no bodies.
#[must_use]
pub fn discover_skills(cwd: &Path, home: &Path) -> Vec<Value> {
    let here = resolve(cwd);
    let root = git_root(&here).unwrap_or_else(|| here.clone());
    let mut skills = Vec::new();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    for (directory, scope) in skill_dirs(&here, &root, home) {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        children.sort();
        for child in children {
            if !child.is_dir() {
                continue;
            }
            let resolved = resolve(&child);
            if seen.contains(&resolved) {
                continue;
            }
            if let Some(entry) = skill_from_dir(&child, scope) {
                seen.insert(resolved);
                skills.push(entry);
            }
        }
    }
    skills
}

/// One skill in full: the first whose name or directory matches.
#[must_use]
pub fn read_skill(name: &str, cwd: &Path, home: &Path) -> Option<Value> {
    let wanted = name.trim();
    if wanted.is_empty() {
        return None;
    }
    for entry in discover_skills(cwd, home) {
        let matches_name = entry["name"].as_str() == Some(wanted);
        let matches_dir = entry["dir"]
            .as_str()
            .map(Path::new)
            .and_then(|d| d.file_name())
            .and_then(|n| n.to_str())
            == Some(wanted);
        if !(matches_name || matches_dir) {
            continue;
        }
        let path = PathBuf::from(entry["path"].as_str().unwrap_or_default());
        let text = read_text(&path);
        let (meta, body) = parse_frontmatter(&text);
        let mut out = entry.as_object().cloned().unwrap_or_default();
        out.insert("body".into(), Value::String(body));
        out.insert(
            "frontmatter".into(),
            Value::Object(
                meta.into_iter()
                    .map(|(k, v)| (k, Value::String(v)))
                    .collect(),
            ),
        );
        out.insert("text".into(), Value::String(text));
        return Some(Value::Object(out));
    }
    None
}

fn ignore_patterns(root: &Path) -> Vec<String> {
    let mut patterns = Vec::new();
    for name in IGNORE_FILE_NAMES {
        let path = root.join(name);
        if !path.is_file() {
            continue;
        }
        for line in read_text(&path).lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            patterns.push(trimmed.to_string());
        }
    }
    patterns
}

/// Whether a repository-relative path is ignored.
#[must_use]
pub fn is_ignored(rel: &str, patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let name = Path::new(rel)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(rel);
    let parts: Vec<&str> = rel.split('/').collect();
    for pattern in patterns {
        let bare = pattern.trim_end_matches('/');
        if crate::glob::matches(rel, pattern) || crate::glob::matches(name, pattern) {
            return true;
        }
        if crate::glob::matches(rel, bare) || crate::glob::matches(name, bare) {
            return true;
        }
        // A trailing slash names a directory, so everything under it goes.
        if pattern.ends_with('/') && (rel == bare || rel.starts_with(&format!("{bare}/"))) {
            return true;
        }
        if parts.contains(&bare) {
            return true;
        }
    }
    false
}

fn git_ls_files(root: &Path) -> Result<Vec<String>, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z"])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() {
            "git ls-files failed".into()
        } else {
            err
        });
    }
    Ok(out
        .stdout
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect())
}

fn tree_outline(files: &[String]) -> Vec<Value> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for rel in files {
        let top = rel.split_once('/').map_or(".", |(head, _)| head);
        *counts.entry(top.to_string()).or_insert(0) += 1;
    }
    // The root bucket sorts last, so a reader sees the directories first.
    let mut keys: Vec<String> = counts.keys().cloned().collect();
    keys.sort_by_key(|k| (k == ".", k.clone()));
    keys.into_iter()
        .map(|key| json!({"path": key, "files": counts[&key]}))
        .collect()
}

/// The git-tracked outline of a checkout. Never a pack write.
#[must_use]
pub fn repo_map(cwd: &Path) -> Value {
    let here = resolve(cwd);
    let root = git_root(&here);
    let gdir = git_dir(&here);
    let git_dir_value = gdir
        .as_ref()
        .map_or(Value::Null, |p| Value::String(p.display().to_string()));

    let Some(root) = root else {
        return json!({
            "status": "failed",
            "reason": "not a git work tree",
            "cwd": here.display().to_string(),
            "root": Value::Null,
            "git_dir": git_dir_value,
            "count": 0,
            "files": [],
            "tree": [],
        });
    };

    let listed = match git_ls_files(&root) {
        Ok(files) => files,
        Err(reason) => {
            return json!({
                "status": "failed",
                "reason": reason,
                "cwd": here.display().to_string(),
                "root": root.display().to_string(),
                "git_dir": git_dir_value,
                "count": 0,
                "files": [],
                "tree": [],
            })
        }
    };
    let patterns = ignore_patterns(&root);
    let files: Vec<String> = listed
        .iter()
        .filter(|rel| !is_ignored(rel, &patterns))
        .cloned()
        .collect();

    let mut payload = json!({
        "cwd": here.display().to_string(),
        "root": root.display().to_string(),
        "git_dir": git_dir_value,
        "count": files.len(),
        "ignored": listed.len() - files.len(),
        "tree": tree_outline(&files),
    });
    let map = payload.as_object_mut().expect("object");
    if files.len() > MAP_TOO_LARGE {
        // A listing nobody can read is worse than the count alone.
        map.insert("status".into(), json!("too-large"));
        map.insert("files".into(), json!([]));
        map.insert("listed".into(), json!(0));
    } else {
        map.insert("status".into(), json!("synced"));
        let shown: Vec<&String> = files.iter().take(MAP_LIST_CAP).collect();
        map.insert("listed".into(), json!(shown.len()));
        map.insert("files".into(), json!(shown));
    }
    payload
}

/// A file's contents when `raw` names one, else `raw` itself, capped.
#[must_use]
pub fn read_attach_source(raw: &str, cap: usize) -> String {
    let text = raw.trim();
    if text.is_empty() {
        return String::new();
    }
    let expanded = if let Some(rest) = text.strip_prefix("~/") {
        std::env::var_os("HOME").map_or_else(
            || PathBuf::from(text),
            |home| PathBuf::from(home).join(rest),
        )
    } else {
        PathBuf::from(text)
    };
    let body = if expanded.is_file() {
        read_text(&expanded)
    } else {
        raw.to_string()
    };
    if body.chars().count() > cap {
        body.chars().take(cap).collect()
    } else {
        body
    }
}

/// The `/v1/rules` answer.
#[must_use]
pub fn rules_payload(cwd: &Path, user_card: &Path, with_body: bool) -> Value {
    let mut rules = discover_rules(cwd, user_card);
    if with_body {
        for rule in &mut rules {
            let path = PathBuf::from(rule["path"].as_str().unwrap_or_default());
            if let Some(map) = rule.as_object_mut() {
                map.insert("text".into(), Value::String(read_text(&path)));
            }
        }
    }
    json!({"rules": rules, "cwd": resolve(cwd).display().to_string()})
}

/// The `/v1/skills` answer, for the catalog or for one skill.
#[must_use]
pub fn skills_payload(cwd: &Path, home: &Path, name: Option<&str>) -> Value {
    let here = resolve(cwd).display().to_string();
    match name {
        Some(wanted) => match read_skill(wanted, cwd, home) {
            Some(skill) => json!({"skills": [skill], "cwd": here, "name": wanted}),
            None => json!({"skills": [], "cwd": here, "name": wanted}),
        },
        None => json!({"skills": discover_skills(cwd, home), "cwd": here}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn frontmatter_is_one_level_and_quotes_come_off() {
        let (meta, body) = parse_frontmatter("---\nname: \"one\"\ndescription: 'two'\n---\nbody\n");
        assert_eq!(meta.get("name").map(String::as_str), Some("one"));
        assert_eq!(meta.get("description").map(String::as_str), Some("two"));
        assert_eq!(body, "body\n");
    }

    #[test]
    fn a_file_with_no_block_is_all_body() {
        let (meta, body) = parse_frontmatter("# A heading\ntext\n");
        assert!(meta.is_empty());
        assert_eq!(body, "# A heading\ntext\n");
    }

    #[test]
    fn an_unterminated_block_is_read_as_prose() {
        // Half-parsing a file that meant to declare a name would put a stray
        // key in the catalog, so the whole thing stays body.
        let (meta, body) = parse_frontmatter("---\nname: one\nno closing marker\n");
        assert!(meta.is_empty());
        assert!(body.starts_with("---"));
    }

    #[test]
    fn the_outline_counts_by_top_level_and_puts_the_root_last() {
        let files: Vec<String> = ["src/a.rs", "src/b.rs", "README.md", "docs/x.md"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let tree = tree_outline(&files);
        let names: Vec<&str> = tree.iter().map(|t| t["path"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["docs", "src", "."]);
        assert_eq!(tree[1]["files"], json!(2));
    }

    #[test]
    fn an_ignore_pattern_matches_a_name_a_path_and_a_directory() {
        let patterns = vec![
            "*.lock".to_string(),
            "target/".to_string(),
            "vendor".to_string(),
        ];
        assert!(is_ignored("Cargo.lock", &patterns));
        assert!(is_ignored("a/b/Cargo.lock", &patterns));
        assert!(is_ignored("target/debug/x", &patterns));
        assert!(is_ignored("a/vendor/b", &patterns), "a path component");
        assert!(!is_ignored("src/main.rs", &patterns));
        assert!(!is_ignored("src/main.rs", &[]));
    }

    /// A repository, or None when git is not on this machine.
    fn git_repo() -> Option<tempfile::TempDir> {
        let dir = tempfile::tempdir().unwrap();
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .arg("init")
            .output()
            .ok()
            .is_some_and(|o| o.status.success());
        ok.then_some(dir)
    }

    #[test]
    fn rules_are_found_most_specific_first_up_to_the_repository_root() {
        let Some(dir) = git_repo() else { return };
        let nested = dir.path().join("a/b");
        fs::create_dir_all(&nested).unwrap();
        fs::write(dir.path().join("AGENTS.md"), "root rules").unwrap();
        fs::write(nested.join("AGENTS.md"), "nested rules").unwrap();

        let card = dir.path().join("nothing.md");
        let found = discover_rules(&nested, &card);
        let scopes: Vec<&str> = found.iter().map(|r| r["scope"].as_str().unwrap()).collect();
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(scopes, vec!["cwd", "root"], "nearest first: {found:?}");
        assert_eq!(found[1]["relpath"], json!("AGENTS.md"));
    }

    #[test]
    fn outside_a_work_tree_the_walk_is_just_the_directory() {
        // The walk stops at the repository root, and with no repository the
        // root is the directory itself. A rule in a parent is somebody else's.
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b");
        fs::create_dir_all(&nested).unwrap();
        fs::write(dir.path().join("AGENTS.md"), "parent rules").unwrap();
        fs::write(nested.join("AGENTS.md"), "nested rules").unwrap();
        if git_root(&nested).is_some() {
            return; // the temp dir landed inside a checkout
        }
        let card = dir.path().join("nothing.md");
        let found = discover_rules(&nested, &card);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0]["scope"], json!("cwd"));
    }

    #[test]
    fn the_seat_card_joins_the_rules_only_when_it_says_something() {
        let dir = tempfile::tempdir().unwrap();
        let card = dir.path().join("USER.md");
        assert!(discover_rules(dir.path(), &card).is_empty());
        fs::write(&card, "   \n").unwrap();
        assert!(
            discover_rules(dir.path(), &card).is_empty(),
            "blank is nothing"
        );
        fs::write(&card, "A standing preference.\n").unwrap();
        let found = discover_rules(dir.path(), &card);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0]["kind"], json!("global"));
    }

    #[test]
    fn a_skill_declares_its_name_or_takes_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let skills = dir.path().join(".agents/skills");
        fs::create_dir_all(skills.join("declared")).unwrap();
        fs::create_dir_all(skills.join("undeclared")).unwrap();
        fs::write(
            skills.join("declared/SKILL.md"),
            "---\nname: a-better-name\ndescription: does a thing\n---\nbody\n",
        )
        .unwrap();
        fs::write(
            skills.join("undeclared/SKILL.md"),
            "# Fallback heading\nbody\n",
        )
        .unwrap();
        fs::write(skills.join("declared/helper.py"), "x").unwrap();

        let home = dir.path().join("nohome");
        let found = discover_skills(dir.path(), &home);
        let by_name: BTreeMap<&str, &Value> = found
            .iter()
            .map(|s| (s["name"].as_str().unwrap(), s))
            .collect();
        assert_eq!(
            by_name["a-better-name"]["description"],
            json!("does a thing")
        );
        assert_eq!(
            by_name["a-better-name"]["supporting"],
            json!(["helper.py"]),
            "SKILL.md itself is not supporting"
        );
        assert_eq!(
            by_name["undeclared"]["description"],
            json!("Fallback heading"),
            "the first heading stands in"
        );
    }

    #[test]
    fn a_directory_without_a_skill_file_is_not_a_skill() {
        let dir = tempfile::tempdir().unwrap();
        let skills = dir.path().join(".agents/skills");
        fs::create_dir_all(skills.join("empty")).unwrap();
        fs::create_dir_all(skills.join("blank")).unwrap();
        fs::write(skills.join("blank/SKILL.md"), "   \n").unwrap();
        let home = dir.path().join("nohome");
        assert!(discover_skills(dir.path(), &home).is_empty());
    }

    #[test]
    fn reading_one_skill_adds_the_body_and_the_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let skills = dir.path().join(".claude/skills/deploy");
        fs::create_dir_all(&skills).unwrap();
        fs::write(
            skills.join("SKILL.md"),
            "---\nname: deploy\ndescription: ship it\n---\nthe steps\n",
        )
        .unwrap();
        let home = dir.path().join("nohome");
        let skill = read_skill("deploy", dir.path(), &home).unwrap();
        assert_eq!(skill["body"], json!("the steps\n"));
        assert_eq!(skill["frontmatter"]["description"], json!("ship it"));
        assert!(skill["text"].as_str().unwrap().starts_with("---"));
        assert!(read_skill("nope", dir.path(), &home).is_none());
        assert!(read_skill("", dir.path(), &home).is_none());
    }

    #[test]
    fn an_attach_source_is_a_path_or_the_text_itself() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("body.txt");
        fs::write(&path, "from the file").unwrap();
        assert_eq!(
            read_attach_source(path.to_str().unwrap(), ATTACH_CAP),
            "from the file"
        );
        assert_eq!(read_attach_source("not a path", ATTACH_CAP), "not a path");
        assert_eq!(read_attach_source("   ", ATTACH_CAP), "");
        assert_eq!(read_attach_source("abcdef", 3), "abc");
    }

    #[test]
    fn a_map_outside_a_work_tree_says_so_rather_than_guessing() {
        let dir = tempfile::tempdir().unwrap();
        let map = repo_map(dir.path());
        // A temp dir may sit inside somebody's checkout, so either answer is
        // legitimate; what matters is that a failure names its reason.
        if map["status"] == json!("failed") {
            assert_eq!(map["reason"], json!("not a git work tree"));
            assert_eq!(map["count"], json!(0));
        } else {
            assert_eq!(map["status"], json!("synced"));
        }
    }
}
