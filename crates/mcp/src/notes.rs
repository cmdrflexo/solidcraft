//! The notes folder (`solidcraft-cli mcp --notes DIR`): a folder of Markdown notes, such as an
//! Obsidian vault, the agent may list and read. Only `.md` files inside the folder (after
//! following links and `..`) are read, up to a size limit; hidden folders (`.git`, `.obsidian`)
//! are skipped. Notes the user keeps from assistants are out of reach: anything in a folder named
//! `Private` (at any level, any case) and any note whose frontmatter says `ai-access: none`.
//! Reading only: nothing here writes.

use std::path::{Component, Path, PathBuf};

use serde_json::{Map, Value, json};

/// The largest note read, in bytes.
pub const MAX_NOTE_BYTES: u64 = 1024 * 1024;
/// Folders by this name (any case, at any level) are never listed or read.
const PRIVATE_FOLDER: &str = "Private";
/// How much of each note `list_notes` reads to find its frontmatter.
const HEAD_BYTES: u64 = 8 * 1024;
/// Most notes one `list_notes` returns.
const MAX_LISTED: usize = 5000;
/// Deepest folder `list_notes` walks into.
const MAX_DEPTH: usize = 32;
/// Most folder entries looked at in one walk (a folder of millions of files stops here).
const MAX_ENTRIES: usize = 200_000;

/// The tools, when a notes folder is set.
pub const TOOLS: &[&str] = &["list_notes", "read_note"];

pub struct Notes {
    /// The folder with links resolved, for the checks.
    root: PathBuf,
    /// The folder as given, for messages (on Windows `root` reads `\\?\C:\…`).
    shown: PathBuf,
}

impl Notes {
    /// The notes folder at `dir`, which must exist.
    pub fn new(dir: &Path) -> Result<Notes, String> {
        let root = std::fs::canonicalize(dir).map_err(|e| format!("notes folder {}: {e}", dir.display()))?;
        if !root.is_dir() {
            return Err(format!("notes folder {}: not a folder", dir.display()));
        }
        Ok(Notes { root, shown: dir.to_path_buf() })
    }

    /// The folder as given.
    pub fn dir(&self) -> &Path {
        &self.shown
    }

    /// What the agent is told in the server's instructions.
    pub fn instructions(&self) -> String {
        format!(
            " The user's notes (Markdown, e.g. an Obsidian vault) are in `{}`: `list_notes` finds them and `read_note` reads one \
             by its path in the folder or its name (as a [[wikilink]] names it). Notes can only be read, and notes the user \
             keeps private (in a Private folder, or marked ai-access: none) can't be read at all.",
            self.shown.display()
        )
    }

    pub fn call(&self, name: &str, a: &Map<String, Value>) -> Result<Value, String> {
        let s = |k: &str| a.get(k).and_then(Value::as_str);
        let n = |k: &str| a.get(k).and_then(Value::as_u64);
        match name {
            "list_notes" => self.list(s("folder").unwrap_or(""), s("filter").unwrap_or("")),
            "read_note" => self.read(s("path").ok_or("`path` is required")?, n("offset"), n("limit")).map(Value::String),
            other => Err(format!("unknown tool `{other}`")),
        }
    }

    /// `folder` (in the notes folder) or the whole folder: every note's path, relative, with `/`.
    fn list(&self, folder: &str, filter: &str) -> Result<Value, String> {
        let asked = self.root.join(folder.trim_matches(['/', '\\']));
        let start = self.inside(&asked)?;
        if self.private(&asked, &start) {
            return Err(format!("`{folder}` is a private folder: the user keeps it from assistants"));
        }
        if !start.is_dir() {
            return Err(format!("not a folder in the notes: {folder}"));
        }
        let filter = filter.to_lowercase();
        let mut notes = Vec::new();
        let mut seen = 0usize;
        let mut truncated = false;
        let mut stack = vec![(start, 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
            entries.sort_by_key(|e| std::cmp::Reverse(e.file_name()));
            for e in entries {
                seen += 1;
                if seen > MAX_ENTRIES || notes.len() >= MAX_LISTED {
                    truncated = true;
                    break;
                }
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                // Links are not followed while walking (a link back up would loop); read_note
                // still reads a linked note that stays inside.
                let Ok(kind) = e.file_type() else { continue };
                let path = e.path();
                if kind.is_dir() {
                    if depth < MAX_DEPTH && !name.eq_ignore_ascii_case(PRIVATE_FOLDER) {
                        stack.push((path, depth + 1));
                    }
                } else if kind.is_file() && is_note(&path) && !head_restricted(&path) {
                    let rel = self.relative(&path);
                    if filter.is_empty() || rel.to_lowercase().contains(&filter) {
                        notes.push(rel);
                    }
                }
            }
            if truncated {
                break;
            }
        }
        notes.sort();
        Ok(json!({"root": self.shown.display().to_string(), "count": notes.len(), "truncated": truncated, "notes": notes}))
    }

    /// The note at `path` (in the notes folder, or absolute inside it), or the one note called
    /// `path` anywhere in it; from line `offset` (1-based), at most `limit` lines.
    fn read(&self, path: &str, offset: Option<u64>, limit: Option<u64>) -> Result<String, String> {
        let note = self.find(path)?;
        let meta = std::fs::metadata(&note).map_err(|e| format!("{path}: {e}"))?;
        if meta.len() > MAX_NOTE_BYTES {
            return Err(format!("{path}: larger than {} KB", MAX_NOTE_BYTES / 1024));
        }
        let text = std::fs::read_to_string(&note).map_err(|e| format!("{path}: {e}"))?;
        if restricted(&text) {
            return Err(format!("`{path}` is marked `ai-access: none`: the user keeps it from assistants"));
        }
        Ok(lines(&text, offset, limit))
    }

    fn find(&self, path: &str) -> Result<PathBuf, String> {
        let path = path.trim();
        if path.is_empty() {
            return Err("`path` is empty".into());
        }
        let asked = Path::new(path);
        let full = if asked.is_absolute() { asked.to_path_buf() } else { self.root.join(path) };
        // As written, then with `.md` added (a wikilink leaves it out).
        let with_ext = PathBuf::from(format!("{}.md", full.display()));
        for candidate in [&full, &with_ext] {
            if candidate.is_file() {
                let p = self.inside(candidate)?;
                if self.private(candidate, &p) {
                    return Err(format!("`{path}` is in a private folder: the user keeps it from assistants"));
                }
                return if is_note(&p) { Ok(p) } else { Err(format!("only Markdown notes (.md) can be read: {path}")) };
            }
        }
        if full.exists() {
            return Err(format!("not a note: {path}"));
        }
        // A bare name: the note with that name anywhere in the folder.
        if !path.contains(['/', '\\']) {
            let stem = path.strip_suffix(".md").unwrap_or(path).to_lowercase();
            let all = self.list("", "")?;
            let hits: Vec<&str> = all["notes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|n| Path::new(n).file_stem().is_some_and(|s| s.to_string_lossy().to_lowercase() == stem))
                .collect();
            match hits.as_slice() {
                [one] => return self.inside(&self.root.join(one)),
                [] => {}
                many => return Err(format!("{} notes are called `{path}`: {}; give the path", many.len(), many.join(", "))),
            }
        }
        Err(format!("no note `{path}` in the notes folder (list_notes shows what there is)"))
    }

    /// `p` with links and `..` resolved, if it is inside the notes folder.
    fn inside(&self, p: &Path) -> Result<PathBuf, String> {
        let c = std::fs::canonicalize(p).map_err(|e| format!("{}: {e}", p.display()))?;
        if c.starts_with(&self.root) { Ok(c) } else { Err(format!("outside the notes folder: {}", p.display())) }
    }

    fn relative(&self, p: &Path) -> String {
        let rel = p.strip_prefix(&self.root).unwrap_or(p);
        rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
    }

    /// Whether a path in the notes folder goes through a `Private` folder, as asked or once
    /// links are resolved (so a link into a private folder is private too, and so is a link
    /// named `Private`). Only the part below the notes folder counts: the notes folder may
    /// itself sit in a folder of that name.
    fn private(&self, asked: &Path, resolved: &Path) -> bool {
        let below = |p: &Path| p.strip_prefix(&self.root).or_else(|_| p.strip_prefix(&self.shown)).ok().map(Path::to_path_buf);
        [asked, resolved]
            .into_iter()
            .filter_map(below)
            .any(|rel| rel.components().any(|c| matches!(c, Component::Normal(n) if n.to_string_lossy().eq_ignore_ascii_case(PRIVATE_FOLDER))))
    }
}

fn is_note(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("md"))
}

/// Whether a note's frontmatter says `ai-access: none`, the user's mark for a note assistants
/// must not read. Quotes, case and a trailing `# comment` don't matter. An unclosed frontmatter
/// counts to the end, so a mark in it still holds.
fn restricted(text: &str) -> bool {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return false;
    }
    for line in lines {
        let line = line.trim_end();
        if line == "---" || line == "..." {
            return false;
        }
        if let Some((key, value)) = line.split_once(':')
            && key.trim().eq_ignore_ascii_case("ai-access")
        {
            let value = value.split(" #").next().unwrap_or("").trim().trim_matches(['"', '\'']);
            if value.eq_ignore_ascii_case("none") {
                return true;
            }
        }
    }
    false
}

/// [`restricted`] from the start of the file alone, for listing. A note that can't be read is
/// listed; `read_note` then says why it can't be read.
fn head_restricted(path: &Path) -> bool {
    use std::io::Read;
    let mut head = Vec::new();
    match std::fs::File::open(path).and_then(|f| f.take(HEAD_BYTES).read_to_end(&mut head)) {
        Ok(_) => restricted(&String::from_utf8_lossy(&head)),
        Err(_) => false,
    }
}

/// Lines `offset..offset + limit` (1-based) of `text`, each with its line ending; all of it without either.
fn lines(text: &str, offset: Option<u64>, limit: Option<u64>) -> String {
    if offset.is_none() && limit.is_none() {
        return text.to_string();
    }
    let skip = usize::try_from(offset.unwrap_or(1).saturating_sub(1)).unwrap_or(usize::MAX);
    let take = limit.map_or(usize::MAX, |l| usize::try_from(l).unwrap_or(usize::MAX));
    text.split_inclusive('\n').skip(skip).take(take).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault(name: &str) -> (PathBuf, Notes) {
        let d = std::env::temp_dir().join(format!("solidcraft-notes-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for dir in ["vault/Projects/Cars", "vault/.obsidian", "vault/Other"] {
            std::fs::create_dir_all(d.join(dir)).unwrap();
        }
        std::fs::write(d.join("vault/me.md"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(d.join("vault/Projects/Cars/Avalanche.MD"), "# Avalanche\n").unwrap();
        std::fs::write(d.join("vault/Projects/Home.md"), "# Projects\n").unwrap();
        std::fs::write(d.join("vault/Other/Home.md"), "# Other\n").unwrap();
        std::fs::write(d.join("vault/.obsidian/hidden.md"), "no").unwrap();
        std::fs::write(d.join("vault/data.json"), "{}").unwrap();
        std::fs::write(d.join("secret.md"), "outside").unwrap();
        let n = Notes::new(&d.join("vault")).unwrap();
        (d, n)
    }

    fn call(n: &Notes, name: &str, args: Value) -> Result<Value, String> {
        n.call(name, args.as_object().unwrap())
    }

    #[test]
    fn notes_are_listed_without_hidden_folders_or_other_files() {
        let (d, n) = vault("list");
        let all = call(&n, "list_notes", json!({})).unwrap();
        assert_eq!(all["notes"], json!(["Other/Home.md", "Projects/Cars/Avalanche.MD", "Projects/Home.md", "me.md"]));
        assert_eq!(all["truncated"], false);
        let sub = call(&n, "list_notes", json!({"folder": "Projects/"})).unwrap();
        assert_eq!(sub["notes"], json!(["Projects/Cars/Avalanche.MD", "Projects/Home.md"]));
        let f = call(&n, "list_notes", json!({"filter": "aVaL"})).unwrap();
        assert_eq!(f["notes"], json!(["Projects/Cars/Avalanche.MD"]));
        assert!(call(&n, "list_notes", json!({"folder": ".."})).unwrap_err().contains("outside"));
        assert!(call(&n, "list_notes", json!({"folder": "me.md"})).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn notes_are_read_by_path_or_by_name_whole_or_by_lines() {
        let (d, n) = vault("read");
        let read = |args: Value| call(&n, "read_note", args).map(|v| v.as_str().unwrap_or("").to_string());
        assert_eq!(read(json!({"path": "me.md"})).unwrap(), "one\ntwo\nthree\n");
        assert_eq!(read(json!({"path": "me"})).unwrap(), "one\ntwo\nthree\n");
        assert_eq!(read(json!({"path": n.dir().join("me.md").to_string_lossy()})).unwrap(), "one\ntwo\nthree\n");
        assert_eq!(read(json!({"path": "Projects/Cars/Avalanche.MD"})).unwrap(), "# Avalanche\n");
        assert_eq!(read(json!({"path": "avalanche"})).unwrap(), "# Avalanche\n", "a wikilink name, in any case");
        assert!(read(json!({"path": "Home"})).unwrap_err().contains("2 notes"), "two notes share the name");
        assert_eq!(read(json!({"path": "me.md", "offset": 2, "limit": 1})).unwrap(), "two\n");
        assert_eq!(read(json!({"path": "me.md", "offset": 3})).unwrap(), "three\n");
        assert_eq!(read(json!({"path": "me.md", "offset": 0, "limit": u64::MAX})).unwrap(), "one\ntwo\nthree\n");
        assert_eq!(read(json!({"path": "me.md", "offset": u64::MAX})).unwrap(), "");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn anything_but_a_note_inside_the_folder_is_refused() {
        let (d, n) = vault("refuse");
        let read = |p: &str| call(&n, "read_note", json!({"path": p}));
        assert!(read("../secret.md").unwrap_err().contains("outside"));
        assert!(read(&d.join("secret.md").to_string_lossy()).unwrap_err().contains("outside"));
        assert!(read("data.json").unwrap_err().contains(".md"));
        assert!(read("Projects").unwrap_err().contains("not a note"));
        assert!(read("gone").unwrap_err().contains("no note"));
        assert!(read(" ").is_err());
        std::fs::write(n.dir().join("big.md"), vec![b'a'; usize::try_from(MAX_NOTE_BYTES).unwrap_or(0) + 1]).unwrap();
        assert!(read("big.md").unwrap_err().contains("larger"));
        assert!(Notes::new(&d.join("nope")).is_err());
        assert!(Notes::new(&d.join("secret.md")).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn private_folders_and_restricted_notes_stay_out_of_reach() {
        let (d, n) = vault("private");
        let v = n.dir().to_path_buf();
        std::fs::create_dir_all(v.join("Private")).unwrap();
        std::fs::create_dir_all(v.join("Projects/private/Deep")).unwrap();
        std::fs::write(v.join("Private/Secret.md"), "# Secret\n").unwrap();
        std::fs::write(v.join("Projects/private/Deep/Hidden.md"), "# Hidden\n").unwrap();
        // A note called Private is not a folder.
        std::fs::write(v.join("Private.md"), "# Private\n").unwrap();
        std::fs::write(v.join("Locked.md"), "---\ncreated: 2026-10-21\nai-access: none\n---\n# Locked\n").unwrap();
        std::fs::write(v.join("Quoted.md"), "\u{feff}---\r\nai-access: \"None\" # keep out\r\n---\r\nbody\r\n").unwrap();
        // Only the frontmatter counts.
        std::fs::write(v.join("Open.md"), "---\nai-access: full\n---\nai-access: none\n").unwrap();
        std::fs::write(v.join("Body.md"), "# Body\n\nai-access: none\n").unwrap();

        let all = call(&n, "list_notes", json!({})).unwrap();
        let listed = all["notes"].to_string();
        for gone in ["Secret", "Hidden", "Locked", "Quoted"] {
            assert!(!listed.contains(gone), "{gone} is listed: {listed}");
        }
        for kept in ["\"Private.md\"", "Open.md", "Body.md", "me.md"] {
            assert!(listed.contains(kept), "{kept} is not listed: {listed}");
        }
        assert!(call(&n, "list_notes", json!({"folder": "Private"})).unwrap_err().contains("private"));
        assert!(call(&n, "list_notes", json!({"folder": "Projects/private/Deep"})).unwrap_err().contains("private"));

        let read = |p: &str| call(&n, "read_note", json!({"path": p}));
        assert!(read("Private/Secret.md").unwrap_err().contains("private"));
        assert!(read("Private/Secret").unwrap_err().contains("private"));
        assert!(read("Projects/../Private/Secret.md").unwrap_err().contains("private"));
        assert!(read(&v.join("Private/Secret.md").to_string_lossy()).unwrap_err().contains("private"));
        assert!(read("Projects/private/Deep/Hidden.md").unwrap_err().contains("private"));
        assert!(read("Secret").unwrap_err().contains("no note"), "a name doesn't find it either");
        assert!(read("Locked.md").unwrap_err().contains("ai-access"));
        assert!(read("Locked").unwrap_err().contains("ai-access"));
        assert!(read("Quoted.md").unwrap_err().contains("ai-access"));
        assert_eq!(read("Private.md").unwrap(), "# Private\n");
        assert_eq!(read("Open.md").unwrap(), "---\nai-access: full\n---\nai-access: none\n");
        assert!(read("Body").is_ok());
        // Part of a restricted note is refused too.
        assert!(call(&n, "read_note", json!({"path": "Locked.md", "offset": 5, "limit": 1})).unwrap_err().contains("ai-access"));

        // A notes folder inside a folder named Private still works: only what is below it counts.
        let nested = d.join("Private/vault");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("note.md"), "ok\n").unwrap();
        let inner = Notes::new(&nested).unwrap();
        assert_eq!(call(&inner, "list_notes", json!({})).unwrap()["notes"], json!(["note.md"]));
        assert_eq!(call(&inner, "read_note", json!({"path": "note"})).unwrap(), "ok\n");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_frontmatter_mark_is_read_leniently_but_only_in_frontmatter() {
        assert!(restricted("---\nai-access: none\n---\n"));
        assert!(restricted("---\nAI-Access:none\n---\n"));
        assert!(restricted("---\ntags:\n  - x\nai-access: 'none'\n---\n"));
        assert!(restricted("---\nai-access: none\n"), "an unclosed frontmatter still counts");
        assert!(!restricted("---\nai-access: read\n---\n"));
        assert!(!restricted("---\n---\nai-access: none\n"));
        assert!(!restricted("# Title\n---\nai-access: none\n---\n"), "frontmatter starts on the first line");
        assert!(!restricted(""));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_into_a_private_folder_is_private() {
        let (d, n) = vault("private-link");
        let v = n.dir().to_path_buf();
        std::fs::create_dir_all(v.join("Private")).unwrap();
        std::fs::write(v.join("Private/Secret.md"), "# Secret\n").unwrap();
        std::os::unix::fs::symlink(v.join("Private"), v.join("Shortcut")).unwrap();
        std::os::unix::fs::symlink(v.join("Private/Secret.md"), v.join("Alias.md")).unwrap();
        // A link named Private counts as a private folder, whatever it points at.
        std::os::unix::fs::symlink(v.join("Projects"), v.join("Other/Private")).unwrap();
        assert!(call(&n, "read_note", json!({"path": "Shortcut/Secret.md"})).unwrap_err().contains("private"));
        assert!(call(&n, "read_note", json!({"path": "Alias.md"})).unwrap_err().contains("private"));
        assert!(call(&n, "read_note", json!({"path": "Other/Private/Home.md"})).unwrap_err().contains("private"));
        assert!(!call(&n, "list_notes", json!({})).unwrap().to_string().contains("Secret"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_out_of_the_folder_is_refused_and_not_walked() {
        let (d, n) = vault("link");
        std::os::unix::fs::symlink(d.join("secret.md"), n.dir().join("link.md")).unwrap();
        std::os::unix::fs::symlink(&d, n.dir().join("up")).unwrap();
        assert!(call(&n, "read_note", json!({"path": "link.md"})).unwrap_err().contains("outside"));
        let all = call(&n, "list_notes", json!({})).unwrap();
        assert!(!all.to_string().contains("secret") && !all.to_string().contains("up/"), "{all}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
