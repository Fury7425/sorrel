//! Content-addressed files for tool output. Rows store a [`BlobRef`], never
//! the bytes, so long outputs stay on disk until someone expands them.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use proto::BlobRef;
use sha2::{Digest, Sha256};

/// Lines of tool output shown before "show all".
pub const PREVIEW_LINES: usize = 20;
const PREVIEW_BYTES: usize = 4000;

pub fn put(dir: &Path, bytes: &[u8]) -> io::Result<BlobRef> {
    let hash: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let blob = BlobRef(hash);
    let path = path(dir, &blob).expect("a sha256 hex digest is a valid ref");
    if !path.exists() {
        fs::create_dir_all(path.parent().expect("blob paths have a parent"))?;
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, bytes)?;
        fs::rename(&tmp, &path)?;
    }
    Ok(blob)
}

/// The file behind `blob`, or `None` if the ref is not a sha256 hex digest
/// (refs arrive from clients, so they must not name arbitrary paths).
pub fn path(dir: &Path, blob: &BlobRef) -> Option<PathBuf> {
    let valid = blob.0.len() == 64 && blob.0.bytes().all(|b| b.is_ascii_hexdigit());
    valid.then(|| dir.join(&blob.0[..2]).join(&blob.0))
}

pub fn read(dir: &Path, blob: &BlobRef) -> io::Result<String> {
    let path = path(dir, blob)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a blob ref"))?;
    Ok(String::from_utf8_lossy(&fs::read(path)?).into_owned())
}

/// Splits tool output into a short preview and, when there is more, a blob
/// holding all of it.
pub fn store_output(dir: &Path, text: &str) -> (String, Option<BlobRef>) {
    let mut lines = text.lines();
    let head = lines
        .by_ref()
        .take(PREVIEW_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    let truncated = lines.next().is_some() || head.len() > PREVIEW_BYTES;
    let preview = crate::clip(&head, PREVIEW_BYTES);
    if !truncated {
        return (preview, None);
    }
    (preview, put(dir, text.as_bytes()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_output_goes_to_a_blob_and_short_output_does_not() {
        let dir = std::env::temp_dir().join(format!("sorrel-blob-{}", std::process::id()));
        let (preview, blob) = store_output(&dir, "one\ntwo");
        assert_eq!((preview.as_str(), blob), ("one\ntwo", None));

        let long: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let (preview, blob) = store_output(&dir, &long);
        assert_eq!(preview.lines().count(), PREVIEW_LINES);
        assert_eq!(read(&dir, &blob.unwrap()).unwrap(), long);
        assert!(path(&dir, &BlobRef("../../etc/passwd".into())).is_none());
    }
}
