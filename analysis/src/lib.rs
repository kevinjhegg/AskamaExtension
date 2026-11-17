//! The analysis behind `askama-lsp`. It knows Askama and Rust but not the Language Server
//! Protocol, so every feature can be tested on plain strings and temporary directories. The
//! server crate only translates between LSP and this crate.

pub mod completion;
pub mod config;
pub mod filters;
pub mod lines;
pub mod lowering;
pub mod navigation;
pub mod outline;
pub mod probe;
pub mod project;
pub mod ranges;
pub mod rust;
pub mod scopes;
pub mod tags;
pub mod template;

#[cfg(test)]
mod testing {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A directory under the system temporary directory, removed when dropped. The name
    /// combines the process id with a counter, so tests running at once never share one.
    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let name = format!("askama-analysis-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
            let path = std::env::temp_dir().join(name);
            std::fs::create_dir_all(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }

        pub fn path(&self, relative: &str) -> PathBuf {
            if relative.is_empty() { self.0.clone() } else { self.0.join(relative) }
        }

        pub fn write(&self, relative: &str, contents: &str) {
            let path = self.path(relative);
            std::fs::create_dir_all(path.parent().unwrap_or(Path::new("/"))).unwrap();
            std::fs::write(path, contents).unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
