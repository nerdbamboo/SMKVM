//! Files on the clipboard: what is offered, where it lands, and the pieces.
//!
//! The exchange carries a *manifest* where the list of files would have
//! gone: relative names and sizes, nothing more. This module builds that
//! manifest from the paths an application copied, reads the pieces the other
//! machines ask for, and -- on the receiving side -- decides where each
//! arriving file may be written, which is the one place a name from another
//! machine touches this machine's disk.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Component, Path, PathBuf};

use smkvm_proto::{FileEntry, FileOffer, TransferId, MAX_CHUNK_DATA};

/// The most entries one copy may carry. A folder with more in it is refused
/// rather than announced a piece at a time for ever.
pub const MAX_ENTRIES: usize = 20_000;

/// A manifest along with where its files actually are, kept by the machine
/// that offered them for as long as the offer stands.
#[derive(Debug, Clone)]
pub struct Offered {
    pub offer: FileOffer,
    /// One per entry, in the manifest's order. Directories have a path too,
    /// though nothing is ever read from them.
    pub paths: Vec<PathBuf>,
}

/// Describe what was copied.
///
/// Files are entered as themselves; a folder is walked and every file under
/// it entered with its path relative to the folder's parent, so the folder
/// arrives as a folder. Symbolic links are not followed -- a link out of the
/// copied tree would carry whatever it points at, which nobody chose to copy.
pub fn describe(id: TransferId, roots: &[PathBuf]) -> std::io::Result<Offered> {
    let mut files = Vec::new();
    let mut paths = Vec::new();
    for root in roots {
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("{} has no name to copy it under", root.display()),
                )
            })?;
        walk(root, &name, &mut files, &mut paths)?;
    }
    let total_bytes = files.iter().map(|f| f.bytes).sum();
    Ok(Offered {
        offer: FileOffer {
            id,
            files,
            total_bytes,
        },
        paths,
    })
}

fn walk(
    path: &Path,
    relative: &str,
    files: &mut Vec<FileEntry>,
    paths: &mut Vec<PathBuf>,
) -> std::io::Result<()> {
    if files.len() >= MAX_ENTRIES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("more than {MAX_ENTRIES} files in one copy"),
        ));
    }
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if meta.is_dir() {
        files.push(FileEntry {
            path: relative.to_owned(),
            bytes: 0,
            is_dir: true,
        });
        paths.push(path.to_path_buf());
        let mut children: Vec<_> = fs::read_dir(path)?.collect::<Result<_, _>>()?;
        children.sort_by_key(|c| c.file_name());
        for child in children {
            let name = child.file_name().to_string_lossy().into_owned();
            walk(&child.path(), &format!("{relative}/{name}"), files, paths)?;
        }
        return Ok(());
    }
    if meta.is_file() {
        files.push(FileEntry {
            path: relative.to_owned(),
            bytes: meta.len(),
            is_dir: false,
        });
        paths.push(path.to_path_buf());
    }
    Ok(())
}

/// One piece of an offered file, for another machine.
///
/// Returns the bytes and whether they are the last. Reads no more than
/// [`MAX_CHUNK_DATA`] so a request is answered by exactly one chunk.
pub fn read_piece(path: &Path, offset: u64) -> std::io::Result<(Vec<u8>, bool)> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if offset > len {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "asked for a piece past the end",
        ));
    }
    file.seek(SeekFrom::Start(offset))?;
    let want = ((len - offset) as usize).min(MAX_CHUNK_DATA);
    let mut buf = vec![0u8; want];
    file.read_exact(&mut buf)?;
    Ok((buf, offset + want as u64 >= len))
}

/// Why a manifest from another machine is not one to write to disk.
#[derive(Debug, PartialEq, Eq)]
pub enum Unacceptable {
    Empty,
    TooLarge {
        bytes: u64,
        limit: u64,
    },
    TooMany(usize),
    /// A name that would land outside the directory it was meant for.
    BadPath(String),
}

impl std::fmt::Display for Unacceptable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unacceptable::Empty => write!(f, "it names no files"),
            Unacceptable::TooLarge { bytes, limit } => write!(
                f,
                "it is {bytes} bytes and the limit is {limit}; raise transfer.max_bytes"
            ),
            Unacceptable::TooMany(n) => write!(f, "it names {n} files, more than can be taken"),
            Unacceptable::BadPath(p) => write!(f, "{p:?} is not a name a file may arrive under"),
        }
    }
}

/// Where a manifest's files will be written, decided once before the first
/// byte arrives so nothing is half done when a name turns out to be bad.
#[derive(Debug)]
pub struct Landing {
    /// One per entry, in the manifest's order.
    pub paths: Vec<PathBuf>,
    /// The paths the pasting application is handed: one per entry the
    /// application named, which is each top-level file or folder.
    pub top_level: Vec<PathBuf>,
}

/// Decide where each file in `offer` lands under `directory`.
///
/// Names are relative, `/`-separated, and may not escape: no absolute paths,
/// no `..`, no drive letters, nothing Windows will not take. A top-level name
/// already present gets a ` (2)` the way a browser's downloads do, so nothing
/// that was there is overwritten. Nothing is created here; that happens as
/// the bytes arrive.
pub fn plan_landing(
    offer: &FileOffer,
    directory: &Path,
    limit: u64,
) -> Result<Landing, Unacceptable> {
    if offer.files.is_empty() {
        return Err(Unacceptable::Empty);
    }
    if offer.files.len() > MAX_ENTRIES {
        return Err(Unacceptable::TooMany(offer.files.len()));
    }
    let bytes: u64 = offer.files.iter().map(|f| f.bytes).sum();
    if bytes > limit || offer.total_bytes > limit {
        return Err(Unacceptable::TooLarge {
            bytes: bytes.max(offer.total_bytes),
            limit,
        });
    }

    // Each top-level name is placed once, and everything beneath it follows.
    let mut placed: HashMap<String, PathBuf> = HashMap::new();
    let mut top_level = Vec::new();
    let mut paths = Vec::with_capacity(offer.files.len());
    for entry in &offer.files {
        let parts =
            safe_parts(&entry.path).ok_or_else(|| Unacceptable::BadPath(entry.path.clone()))?;
        let (first, rest) = parts.split_first().expect("safe_parts never returns empty");
        let base = match placed.get(*first) {
            Some(base) => base.clone(),
            None => {
                let base = unclaimed(directory, first);
                placed.insert((*first).to_owned(), base.clone());
                top_level.push(base.clone());
                base
            }
        };
        let mut path = base;
        for part in rest {
            path.push(part);
        }
        paths.push(path);
    }
    Ok(Landing { paths, top_level })
}

/// The components of a relative name, or `None` if it is not one this will
/// write. Empty and `.` components are skipped; `..`, roots, prefixes and
/// characters no filesystem here accepts are refused outright.
fn safe_parts(name: &str) -> Option<Vec<&str>> {
    if name.contains('\0') || name.contains('\\') || name.starts_with('/') {
        return None;
    }
    let mut parts = Vec::new();
    for part in name.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." || part.ends_with('.') || part.ends_with(' ') {
            return None;
        }
        if part
            .chars()
            .any(|c| c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        {
            return None;
        }
        // Belt and braces: what std makes of it must be one plain name.
        let mut components = Path::new(part).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(_)), None) => {}
            _ => return None,
        }
        parts.push(part);
    }
    (!parts.is_empty()).then_some(parts)
}

/// `name`, or `name (2)`, `name (3)`... -- the first not already present.
fn unclaimed(directory: &Path, name: &str) -> PathBuf {
    let first = directory.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, Some(ext)),
        _ => (name, None),
    };
    for n in 2..10_000 {
        let candidate = match ext {
            Some(ext) => format!("{stem} ({n}).{ext}"),
            None => format!("{stem} ({n})"),
        };
        let path = directory.join(candidate);
        if !path.exists() {
            return path;
        }
    }
    first
}

/// A file being written as its pieces arrive.
pub struct Arriving {
    file: File,
    written: u64,
}

impl Arriving {
    /// Create the file, and every directory above it.
    pub fn create(path: &Path) -> std::io::Result<Arriving> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(Arriving {
            file: File::create(path)?,
            written: 0,
        })
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    /// Append a piece, which must begin exactly where the last one ended.
    pub fn append(&mut self, offset: u64, data: &[u8]) -> std::io::Result<()> {
        if offset != self.written {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "a piece arrived out of order",
            ));
        }
        self.file.write_all(data)?;
        self.written += data.len() as u64;
        Ok(())
    }

    pub fn finish(mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smkvm_layout::DeviceId;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "smkvm-transfer-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn id() -> TransferId {
        TransferId {
            device: DeviceId::from_bytes([9; 32]),
            counter: 1,
        }
    }

    #[test]
    fn a_folder_is_described_with_everything_under_it_and_read_back_in_pieces() {
        let dir = scratch("describe");
        let folder = dir.join("photos");
        fs::create_dir_all(folder.join("2024")).unwrap();
        fs::write(folder.join("2024/a.jpg"), vec![1u8; 10]).unwrap();
        fs::write(folder.join("b.txt"), b"hello").unwrap();
        fs::write(dir.join("loose.bin"), vec![7u8; MAX_CHUNK_DATA + 3]).unwrap();

        let offered = describe(id(), &[folder.clone(), dir.join("loose.bin")]).unwrap();
        let names: Vec<(&str, u64, bool)> = offered
            .offer
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.bytes, f.is_dir))
            .collect();
        assert_eq!(
            names,
            vec![
                ("photos", 0, true),
                ("photos/2024", 0, true),
                ("photos/2024/a.jpg", 10, false),
                ("photos/b.txt", 5, false),
                ("loose.bin", MAX_CHUNK_DATA as u64 + 3, false),
            ]
        );
        assert_eq!(offered.offer.total_bytes, 15 + MAX_CHUNK_DATA as u64 + 3);
        assert_eq!(offered.paths.len(), offered.offer.files.len());

        // The big one takes two pieces; the second is the last.
        let (first, last) = read_piece(&offered.paths[4], 0).unwrap();
        assert_eq!((first.len(), last), (MAX_CHUNK_DATA, false));
        let (second, last) = read_piece(&offered.paths[4], MAX_CHUNK_DATA as u64).unwrap();
        assert_eq!((second.len(), last), (3, true));
        assert!(read_piece(&offered.paths[4], MAX_CHUNK_DATA as u64 + 4).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_link_is_not_followed() {
        #[cfg(unix)]
        {
            let dir = scratch("link");
            fs::write(dir.join("real"), b"x").unwrap();
            std::os::unix::fs::symlink("/etc/passwd", dir.join("sneaky")).unwrap();
            let offered = describe(id(), std::slice::from_ref(&dir)).unwrap();
            let names: Vec<&str> = offered
                .offer
                .files
                .iter()
                .map(|f| f.path.as_str())
                .collect();
            assert!(names.iter().any(|n| n.ends_with("/real")));
            assert!(!names.iter().any(|n| n.ends_with("/sneaky")));
            let _ = fs::remove_dir_all(dir);
        }
    }

    fn offer_of(names: &[(&str, u64, bool)]) -> FileOffer {
        FileOffer {
            id: id(),
            files: names
                .iter()
                .map(|(p, b, d)| FileEntry {
                    path: (*p).to_owned(),
                    bytes: *b,
                    is_dir: *d,
                })
                .collect(),
            total_bytes: names.iter().map(|(_, b, _)| *b).sum(),
        }
    }

    #[test]
    fn names_that_would_escape_are_refused_before_anything_is_written() {
        let dir = scratch("escape");
        for bad in [
            "../etc/passwd",
            "a/../../b",
            "/etc/passwd",
            "C:/Windows/x",
            "a\\b",
            "nul\0byte",
            "trailing.",
            "what?",
            "",
        ] {
            let offer = offer_of(&[(bad, 1, false)]);
            let result = plan_landing(&offer, &dir, 1 << 30);
            assert!(
                matches!(
                    result,
                    Err(Unacceptable::BadPath(_)) | Err(Unacceptable::Empty)
                ),
                "{bad:?} was accepted: {result:?}"
            );
        }
        // Harmless oddities are tidied rather than refused.
        let offer = offer_of(&[("./a//b.txt", 1, false)]);
        let landing = plan_landing(&offer, &dir, 1 << 30).unwrap();
        assert_eq!(landing.paths, vec![dir.join("a").join("b.txt")]);
        assert_eq!(landing.top_level, vec![dir.join("a")]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn what_is_already_there_is_not_overwritten() {
        let dir = scratch("collide");
        fs::write(dir.join("report.pdf"), b"old").unwrap();
        fs::write(dir.join("report (2).pdf"), b"older").unwrap();
        fs::create_dir(dir.join("photos")).unwrap();
        let offer = offer_of(&[
            ("report.pdf", 3, false),
            ("photos", 0, true),
            ("photos/a.jpg", 1, false),
        ]);
        let landing = plan_landing(&offer, &dir, 1 << 30).unwrap();
        assert_eq!(
            landing.paths,
            vec![
                dir.join("report (3).pdf"),
                dir.join("photos (2)"),
                dir.join("photos (2)").join("a.jpg"),
            ]
        );
        assert_eq!(
            landing.top_level,
            vec![dir.join("report (3).pdf"), dir.join("photos (2)")]
        );
        assert_eq!(fs::read(dir.join("report.pdf")).unwrap(), b"old");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn too_much_is_refused_with_the_limit_named() {
        let dir = scratch("limit");
        let offer = offer_of(&[("big.iso", 10, false)]);
        assert!(matches!(
            plan_landing(&offer, &dir, 9),
            Err(Unacceptable::TooLarge {
                bytes: 10,
                limit: 9
            })
        ));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn pieces_are_written_in_order_and_only_in_order() {
        let dir = scratch("arrive");
        let path = dir.join("deep").join("er").join("file.bin");
        let mut arriving = Arriving::create(&path).unwrap();
        arriving.append(0, b"hel").unwrap();
        assert!(arriving.append(2, b"x").is_err(), "a repeat is refused");
        assert!(arriving.append(4, b"x").is_err(), "a gap is refused");
        arriving.append(3, b"lo").unwrap();
        assert_eq!(arriving.written(), 5);
        arriving.finish().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"hello");
        let _ = fs::remove_dir_all(dir);
    }
}
