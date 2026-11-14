//! Code and command examples use spaces.
//!
//! A transcript used to mix a tab-separated row with a space-separated
//! command, and the two copies of a page did not agree on the gap. Spaces
//! throughout, two between the fields a command prints. A tab in an example
//! is the mix coming back.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn walk(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in
        std::fs::read_dir(dir).unwrap_or_else(|err| panic!("read {}: {err}", dir.display()))
    {
        let path = entry
            .unwrap_or_else(|err| panic!("read {}: {err}", dir.display()))
            .path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "build") {
                continue;
            }
            walk(&path, files);
        } else if path
            .extension()
            .is_some_and(|ext| ext == "rst" || ext == "org" || ext == "md")
        {
            files.push(path);
        }
    }
}

fn pages() -> Vec<PathBuf> {
    let root = repo_root();
    let mut files = vec![root.join("README.md")];
    walk(&root.join("docs"), &mut files);
    files.sort();
    files
}

fn indented(line: &str) -> bool {
    line.starts_with(' ') || line.starts_with('\t')
}

fn rst_blocks(text: &str) -> Vec<Vec<&str>> {
    let lines: Vec<&str> = text.lines().collect();
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].starts_with(".. code::") || lines[i].starts_with(".. code-block::") {
            i += 1;
            while i < lines.len() {
                let option = lines[i].trim_start().starts_with(':') && indented(lines[i]);
                if option {
                    i += 1;
                    continue;
                }
                if lines[i].trim().is_empty() {
                    let content = i + 1 < lines.len() && indented(lines[i + 1]);
                    i += 1;
                    if content {
                        break;
                    }
                    continue;
                }
                break;
            }
            let mut block = Vec::new();
            while i < lines.len() {
                if lines[i].trim().is_empty() {
                    if i + 1 < lines.len() && indented(lines[i + 1]) {
                        block.push(lines[i]);
                        i += 1;
                        continue;
                    }
                    break;
                }
                if indented(lines[i]) {
                    block.push(lines[i]);
                    i += 1;
                    continue;
                }
                break;
            }
            blocks.push(block);
            continue;
        }
        i += 1;
    }
    blocks
}

fn org_blocks(text: &str) -> Vec<Vec<&str>> {
    let lines: Vec<&str> = text.lines().collect();
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].to_ascii_lowercase().starts_with("#+begin_src") {
            i += 1;
            let mut block = Vec::new();
            while i < lines.len() && !lines[i].to_ascii_lowercase().starts_with("#+end_src") {
                block.push(lines[i]);
                i += 1;
            }
            blocks.push(block);
        }
        i += 1;
    }
    blocks
}

fn md_blocks(text: &str) -> Vec<Vec<&str>> {
    let lines: Vec<&str> = text.lines().collect();
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].starts_with("```") {
            i += 1;
            let mut block = Vec::new();
            while i < lines.len() && !lines[i].starts_with("```") {
                block.push(lines[i]);
                i += 1;
            }
            if i < lines.len() {
                i += 1;
            }
            blocks.push(block);
            continue;
        }
        i += 1;
    }
    blocks
}

fn blocks_of<'a>(path: &Path, text: &'a str) -> Vec<Vec<&'a str>> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("rst") => rst_blocks(text),
        Some("org") => org_blocks(text),
        Some("md") => md_blocks(text),
        other => panic!("{} has no example parser ({other:?})", path.display()),
    }
}

fn marker_count(text: &str, path: &Path) -> usize {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("rst") => text
            .lines()
            .filter(|line| line.starts_with(".. code::") || line.starts_with(".. code-block::"))
            .count(),
        Some("org") => text
            .lines()
            .filter(|line| line.to_ascii_lowercase().starts_with("#+begin_src"))
            .count(),
        Some("md") => {
            let fences = text.lines().filter(|line| line.starts_with("```")).count();
            assert!(fences % 2 == 0, "{} has an open fence", path.display());
            fences / 2
        }
        _ => 0,
    }
}

#[test]
fn code_and_command_examples_use_spaces() {
    let root = repo_root();
    let mut seen = 0usize;
    for path in pages() {
        let text = std::fs::read_to_string(&path).unwrap_or_else(|err| {
            panic!("read {}: {err}", path.display());
        });
        let blocks = blocks_of(&path, &text);
        let markers = marker_count(&text, &path);
        assert_eq!(
            blocks.len(),
            markers,
            "{} parsed {} examples, the page marks {markers}",
            path.display(),
            blocks.len()
        );
        for block in blocks {
            seen += 1;
            for line in block {
                assert!(
                    !line.contains('\t'),
                    "{}: a code example contains a tab: {line:?}",
                    path.strip_prefix(&root).unwrap_or(&path).display()
                );
            }
        }
    }
    assert!(seen > 20, "the check scanned {seen} examples");
}
