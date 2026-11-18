//! Locating the crate a template belongs to, and reading and walking the file system within limits.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

/// The directory of the nearest `Cargo.toml` above a file: the crate whose `CARGO_MANIFEST_DIR` Askama resolves `askama.toml` and template
/// directories against.
pub fn crate_root(file: &Path) -> Option<PathBuf> {
    file.ancestors()
        .skip(1)
        .find(|dir| dir.join("Cargo.toml").is_file())
        .map(Path::to_path_buf)
}

/// The largest file the analysis reads from disk. A template or a Rust file is far smaller; a larger file is skipped, not read into memory.
pub const MAX_FILE_SIZE: usize = 8 * 1024 * 1024;

/// The text of a regular file of at most `MAX_FILE_SIZE` bytes. `None` for anything else: a file that cannot be read or is not UTF-8, a
/// larger file, and what is not a regular file, such as a named pipe, which would never finish reading.
pub fn read_text(path: &Path) -> Option<String> {
    read_text_or_error(path).ok()
}

/// Like `read_text`, saying why a file was not read.
///
/// # Errors
///
/// The error from the file system (`NotFound` for a file that does not exist), `InvalidInput` for what is not a regular file, and
/// `InvalidData` for a file that is larger than `MAX_FILE_SIZE` or not UTF-8.
pub fn read_text_or_error(path: &Path) -> std::io::Result<String> {
    if !std::fs::metadata(path)?.is_file() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a regular file"));
    }
    let file = std::fs::File::open(path)?;
    let mut text = String::new();
    // One byte more than the limit is read, to tell a file of exactly the limit from a larger one.
    file.take(u64::try_from(MAX_FILE_SIZE + 1).unwrap_or(u64::MAX)).read_to_string(&mut text)?;
    if text.len() > MAX_FILE_SIZE {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("larger than {MAX_FILE_SIZE} bytes")));
    }
    Ok(text)
}

/// The most files a walk over a crate's template directories or its `src` collects. A crate has far fewer; the limit keeps a template
/// directory that names something much larger, such as the whole disk, from costing more than this.
pub const MAX_WALKED_FILES: usize = 10_000;

/// The directories a walk over the file system has entered, by canonical path, so a symbolic link back to a directory already walked is not
/// walked again. Without it, a few such links nested a few levels deep multiply the work at every level.
#[derive(Debug, Default)]
pub struct Visited(HashSet<PathBuf>);

impl Visited {
    /// Whether `dir` is entered for the first time, recording it. A directory whose canonical path cannot be found is not entered.
    pub fn enter(&mut self, dir: &Path) -> bool {
        dir.canonicalize().is_ok_and(|canonical| self.0.insert(canonical))
    }
}

/// Every crate under a workspace folder: each directory with a `Cargo.toml`, not looking in hidden directories, `target` or `node_modules`,
/// nor deeper than `MAX_DEPTH`, and entering each directory once.
pub fn crates_under(folder: &Path) -> Vec<PathBuf> {
    /// How deep below the folder the search goes.
    const MAX_DEPTH: usize = 8;
    /// Adds the crates at and under `dir` to `found`.
    fn walk(dir: &Path, depth: usize, visited: &mut Visited, found: &mut Vec<PathBuf>) {
        if !visited.enter(dir) {
            return;
        }
        if dir.join("Cargo.toml").is_file() {
            found.push(dir.to_path_buf());
        }
        if depth == MAX_DEPTH {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut dirs: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_dir())
            .filter(|path| {
                let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
                !(name.starts_with('.') || name == "target" || name == "node_modules")
            })
            .collect();
        dirs.sort();
        for dir in dirs {
            walk(&dir, depth + 1, visited, found);
        }
    }
    let mut found = Vec::new();
    walk(folder, 0, &mut Visited::default(), &mut found);
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    #[test]
    fn finds_crates_but_not_in_build_output() {
        let dir = TempDir::new();
        dir.write("Cargo.toml", "");
        dir.write("a/Cargo.toml", "");
        dir.write("a/b/Cargo.toml", "");
        dir.write("target/c/Cargo.toml", "");
        dir.write(".hidden/Cargo.toml", "");
        assert_eq!(crates_under(&dir.path("")), [dir.path(""), dir.path("a"), dir.path("a/b")]);
    }

    #[test]
    fn reads_only_regular_files_within_the_size_limit() {
        let dir = TempDir::new();
        dir.write("small.html", "{{ x }}");
        dir.write("large.html", &"x".repeat(MAX_FILE_SIZE + 1));
        dir.write("dir/inside.html", "");
        assert_eq!(read_text(&dir.path("small.html")).as_deref(), Some("{{ x }}"));
        assert_eq!(read_text(&dir.path("large.html")), None);
        assert_eq!(read_text_or_error(&dir.path("large.html")).unwrap_err().kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(read_text_or_error(&dir.path("dir")).unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(read_text_or_error(&dir.path("missing.html")).unwrap_err().kind(), std::io::ErrorKind::NotFound);
    }

    #[cfg(unix)]
    #[test]
    fn enters_each_directory_once_through_symbolic_links() {
        use std::os::unix::fs::symlink;
        let dir = TempDir::new();
        dir.write("Cargo.toml", "");
        dir.write("a/Cargo.toml", "");
        // Three links back to the top at each level would make 3^8 directories to walk without the visited set.
        for name in ["x", "y", "z"] {
            symlink(dir.path(""), dir.path(name)).unwrap();
            symlink(dir.path(""), dir.path(&format!("a/{name}"))).unwrap();
        }
        assert_eq!(crates_under(&dir.path("")), [dir.path(""), dir.path("a")]);
    }

    #[test]
    fn finds_the_nearest_manifest() {
        let dir = TempDir::new();
        dir.write("Cargo.toml", "");
        dir.write("nested/Cargo.toml", "");
        dir.write("nested/templates/a/b.html", "");
        dir.write("templates/c.html", "");
        assert_eq!(crate_root(&dir.path("nested/templates/a/b.html")), Some(dir.path("nested")));
        assert_eq!(crate_root(&dir.path("templates/c.html")), Some(dir.path("")));
    }
}
