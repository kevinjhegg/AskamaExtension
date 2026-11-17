//! Locating the crate a template belongs to.

use std::path::{Path, PathBuf};

/// The directory of the nearest `Cargo.toml` above a file: the crate whose `CARGO_MANIFEST_DIR`
/// Askama resolves `askama.toml` and template directories against.
pub fn crate_root(file: &Path) -> Option<PathBuf> {
    file.ancestors()
        .skip(1)
        .find(|dir| dir.join("Cargo.toml").is_file())
        .map(Path::to_path_buf)
}

/// Every crate under a workspace folder: each directory with a `Cargo.toml`, not looking in
/// hidden directories, `target` or `node_modules`, nor deeper than `MAX_DEPTH`.
pub fn crates_under(folder: &Path) -> Vec<PathBuf> {
    const MAX_DEPTH: usize = 8;
    fn walk(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
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
            walk(&dir, depth + 1, found);
        }
    }
    let mut found = Vec::new();
    walk(folder, 0, &mut found);
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
