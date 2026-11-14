//! Port of `apps/api/app/integrations/storage/__init__.py`: object storage for uploads and owner
//! recordings, on the local filesystem (`OBJECT_STORAGE_LOCAL_DIR`, gitignored).
//!
//! Keys are `{project_id}/{sha256(data)[:16]}-{safe filename}`, where the safe filename keeps
//! alphanumeric characters and `._-` (at most 80 characters, `upload` when nothing is left).
//! `get` refuses any key that does not resolve to a file strictly inside the root (symlinks are
//! resolved first), so `..`, absolute keys and links out of the root are rejected.
//!
//! Differences from Python: a key that resolves to a directory inside the root is "not found"
//! (Python raises `IsADirectoryError`, a 500). Python's `str.isalnum` and Rust's
//! `char::is_alphanumeric` differ on a few exotic code points (combining marks, some numerals).

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// Python's `ValueError("invalid key")` / `FileNotFoundError`: the route answers 404.
    #[error("object not found")]
    NotFound,
    #[error("object storage I/O failed: {0}")]
    Io(String),
}

pub trait ObjectStorage: Send + Sync {
    fn put(
        &self,
        project_id: &str,
        filename: &str,
        data: &[u8],
        content_type: &str,
    ) -> Result<String, StorageError>;
    fn get(&self, key: &str) -> Result<Vec<u8>, StorageError>;
}

#[derive(Debug, Clone)]
pub struct LocalObjectStorage {
    pub root: PathBuf,
}

impl LocalObjectStorage {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

/// The filename part of a key.
pub fn safe_filename(filename: &str) -> String {
    let safe: String = filename
        .chars()
        .filter(|c| c.is_alphanumeric() || "._-".contains(*c))
        .take(80)
        .collect();
    if safe.is_empty() {
        "upload".to_owned()
    } else {
        safe
    }
}

/// `Path.resolve()` of a path that may not exist: absolute, symlinks resolved as far as the
/// path exists, `.` and `..` removed lexically beyond that.
fn resolve(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|d| d.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    };
    if let Ok(c) = abs.canonicalize() {
        return c;
    }
    let mut out = PathBuf::new();
    for comp in abs.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

impl ObjectStorage for LocalObjectStorage {
    fn put(
        &self,
        project_id: &str,
        filename: &str,
        data: &[u8],
        _content_type: &str,
    ) -> Result<String, StorageError> {
        let digest = hex::encode(Sha256::digest(data));
        let key = format!("{project_id}/{}-{}", &digest[..16], safe_filename(filename));
        let path = self.root.join(&key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StorageError::Io(e.to_string()))?;
        }
        std::fs::write(&path, data).map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(key)
    }

    fn get(&self, key: &str) -> Result<Vec<u8>, StorageError> {
        let root = resolve(&self.root);
        // `(root / key).resolve()`; an absolute key replaces the root, as `Path /` does.
        let path = resolve(&self.root.join(key));
        if path == root || !path.starts_with(&root) {
            return Err(StorageError::NotFound);
        }
        if !path.is_file() {
            return Err(StorageError::NotFound);
        }
        std::fs::read(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => StorageError::NotFound,
            _ => StorageError::Io(e.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("ss-storage-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn put_get_and_key_layout() {
        let root = tmp();
        let s = LocalObjectStorage::new(&root);
        let key = s
            .put("p1", "../../etc/pass wd.mp3", b"abc", "audio/mpeg")
            .unwrap();
        // sha256("abc") = ba7816bf8f01cfea...
        assert_eq!(key, "p1/ba7816bf8f01cfea-....etcpasswd.mp3");
        assert_eq!(s.get(&key).unwrap(), b"abc");
        assert_eq!(safe_filename("///"), "upload");
        assert_eq!(safe_filename(&"x".repeat(100)).len(), 80);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn traversal_is_refused() {
        let root = tmp();
        let inner = root.join("objects");
        std::fs::create_dir_all(inner.join("p1")).unwrap();
        std::fs::write(root.join("secret.txt"), b"s").unwrap();
        std::fs::write(inner.join("p1").join("ok.bin"), b"ok").unwrap();
        let s = LocalObjectStorage::new(&inner);
        assert_eq!(s.get("p1/ok.bin").unwrap(), b"ok");
        for bad in [
            "../secret.txt",
            "p1/../../secret.txt",
            root.join("secret.txt").to_str().unwrap(),
            "",
            ".",
            "p1",
            "p1/missing.bin",
        ] {
            assert!(
                matches!(s.get(bad), Err(StorageError::NotFound)),
                "{bad:?} was served"
            );
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("secret.txt"), inner.join("p1").join("link"))
                .unwrap();
            assert!(matches!(s.get("p1/link"), Err(StorageError::NotFound)));
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
