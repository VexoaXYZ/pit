//! Content-addressed blob store on local disk, keyed by BLAKE3 hex digest.
//! Used by the server (authoritative) and by each worker (node-local cache).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

#[derive(Clone)]
pub struct Cas {
    root: PathBuf,
}

pub fn digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

pub fn is_digest(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

impl Cas {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(root.join("tmp"))
            .with_context(|| format!("creating CAS at {}", root.display()))?;
        Ok(Cas { root })
    }

    pub fn path(&self, digest: &str) -> PathBuf {
        self.root.join(&digest[..2]).join(&digest[2..])
    }

    pub fn contains(&self, digest: &str) -> bool {
        is_digest(digest) && self.path(digest).exists()
    }

    pub fn get(&self, digest: &str) -> Result<Option<Vec<u8>>> {
        if !is_digest(digest) {
            bail!("invalid digest {digest:?}");
        }
        match std::fs::read(self.path(digest)) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Stores bytes and returns their digest.
    pub fn put(&self, bytes: &[u8]) -> Result<String> {
        let d = digest(bytes);
        self.put_verified(&d, bytes)?;
        Ok(d)
    }

    /// Stores bytes under a digest the caller claims, after checking it.
    pub fn put_checked(&self, claimed: &str, bytes: &[u8]) -> Result<()> {
        let d = digest(bytes);
        if d != claimed {
            bail!("digest mismatch: claimed {claimed}, content is {d}");
        }
        self.put_verified(&d, bytes)
    }

    fn put_verified(&self, d: &str, bytes: &[u8]) -> Result<()> {
        let dest = self.path(d);
        if dest.exists() {
            return Ok(());
        }
        std::fs::create_dir_all(dest.parent().unwrap())?;
        let tmp = self.root.join("tmp").join(crate::model::new_id("put"));
        std::fs::write(&tmp, bytes)?;
        // Rename is atomic, so readers never see a partial blob.
        if let Err(e) = std::fs::rename(&tmp, &dest) {
            let _ = std::fs::remove_file(&tmp);
            if !dest.exists() {
                return Err(e.into());
            }
        }
        Ok(())
    }

    pub fn put_file(&self, path: &Path) -> Result<String> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        self.put(&bytes)
    }

    /// Number of blobs and total bytes. Walks the store, so it is not cheap.
    pub fn stats(&self) -> (u64, u64) {
        let mut count = 0;
        let mut bytes = 0;
        if let Ok(dirs) = std::fs::read_dir(&self.root) {
            for dir in dirs.flatten() {
                if dir.file_name() == "tmp" {
                    continue;
                }
                if let Ok(files) = std::fs::read_dir(dir.path()) {
                    for f in files.flatten() {
                        count += 1;
                        bytes += f.metadata().map(|m| m.len()).unwrap_or(0);
                    }
                }
            }
        }
        (count, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_roundtrip_and_verification() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::open(dir.path()).unwrap();
        let d = cas.put(b"hello").unwrap();
        assert_eq!(d, digest(b"hello"));
        assert!(cas.contains(&d));
        assert_eq!(cas.get(&d).unwrap().unwrap(), b"hello");
        assert!(cas.put_checked(&d, b"nope").is_err());
        assert!(cas.get(&digest(b"missing")).unwrap().is_none());
        assert!(cas.get("../etc/passwd").is_err());
    }
}
