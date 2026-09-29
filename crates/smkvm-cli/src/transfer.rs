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
    ///
    /// Only the dangerous kind now. A name that is merely awkward --
    /// a line break in it, a character Windows reserves -- is cleaned
    /// and the file arrives; see `usable_parts`.
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
            Unacceptable::BadPath(p) => write!(
                f,
                "{p:?} could land outside the directory it was meant for, so none of these                  files were taken"
            ),
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
    /// Names that could not be written as they were sent, as they were
    /// and as they will be. Said once per file by whoever lands them,
    /// so that a person can tell why what they pasted is not called
    /// quite what it was.
    pub renamed: Vec<(String, String)>,
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
    let mut renamed = Vec::new();
    for entry in &offer.files {
        let named =
            usable_parts(&entry.path).ok_or_else(|| Unacceptable::BadPath(entry.path.clone()))?;
        for change in named.changed {
            if !renamed.contains(&change) {
                renamed.push(change);
            }
        }
        let (first, rest) = named
            .parts
            .split_first()
            .expect("usable_parts never returns empty");
        let base = match placed.get(first) {
            Some(base) => base.clone(),
            None => {
                let base = unclaimed(directory, first);
                placed.insert(first.clone(), base.clone());
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
    Ok(Landing {
        paths,
        top_level,
        renamed,
    })
}

/// The longest one component of a name may be.
///
/// Windows allows 255 units per component; two hundred leaves room for
/// the ` (2)` a clash adds and for the directory in front of it, and is
/// still far longer than anything a person types.
const LONGEST_PART: usize = 200;

/// What is used when cleaning leaves nothing at all -- a name that was
/// only dots, or only spaces.
const UNNAMED: &str = "unnamed";

/// Device names Windows will not let a file have, whatever the
/// extension after them.
const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// A name, and what had to be done to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    /// The components to write under, cleaned where they had to be.
    pub parts: Vec<String>,
    /// Every part that is not what was sent, as it was and as it will
    /// be. One line in the log per file, so a person can tell why the
    /// thing they pasted is not called quite what it was.
    pub changed: Vec<(String, String)>,
}

/// The components of a relative name, refusing what is dangerous and
/// cleaning what is merely unwritable.
///
/// These two were the same thing once, and treating them the same is a
/// fault that cost a person a week of file transfers. A paper
/// downloaded from a journal came with a line break inside its
/// filename -- which happens constantly -- and every attempt to copy it
/// was abandoned, with the only trace a warning in the log of the
/// machine doing the *sending*, which is the one place nobody looks.
/// Losing a newline out of a filename is not a loss worth refusing a
/// file for.
///
/// So the two kinds are separated by what is at stake.
///
/// **Refused**, because nothing about the name can be trusted and
/// writing it might not write where it was meant to: `..`, a leading
/// `/`, a backslash, a NUL, a drive letter, anything `Path::components`
/// does not make exactly one ordinary component of. These are the ways
/// a name escapes the directory it was given, and a name that might
/// escape is not a name to be tidied up and used anyway.
///
/// **Cleaned**, because the file is fine and only its label is
/// awkward: control characters, the characters Windows reserves,
/// trailing dots and spaces, the reserved device names, and anything
/// too long. The file arrives, under a name that is as close as can be
/// written, and the change is reported.
pub fn usable_parts(name: &str) -> Option<Named> {
    // Refusals first, on the whole name, before any part of it is
    // looked at as something to salvage.
    // A colon is refused rather than cleaned, and that distinction is
    // the whole rule doing its job. It is not an awkward character: on
    // Windows it means a drive (`C:\x`) or an alternate data stream
    // (`notes.txt:hidden`), both of which are "this names something
    // other than a file here". Cleaning it to `_` would turn `C:` into
    // the perfectly ordinary-looking `C_` -- which is exactly the way
    // this change could have made things worse rather than better, and
    // is how the existing escape test caught it.
    if name.contains('\0') || name.contains('\\') || name.starts_with('/') || name.contains(':') {
        return None;
    }
    let mut parts = Vec::new();
    let mut changed = Vec::new();
    for part in name.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return None;
        }
        // What std makes of the part as given must be one plain name.
        // Checked before cleaning, so that cleaning can never turn
        // something that was a root or a prefix into something that
        // looks ordinary.
        let mut components = Path::new(part).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(_)), None) => {}
            _ => return None,
        }
        let cleaned = clean_part(part);
        if cleaned != part {
            changed.push((part.to_string(), cleaned.clone()));
        }
        parts.push(cleaned);
    }
    (!parts.is_empty()).then_some(Named { parts, changed })
}

/// One component, made into something this platform will accept.
///
/// Nothing here may produce an empty string, a `.`, a `..`, or anything
/// with a separator in it -- the refusals above are what keep a name
/// inside its directory, and cleaning must not undo them.
fn clean_part(part: &str) -> String {
    // A control character becomes a space rather than nothing, because
    // it is almost always standing where a space belonged: a filename
    // that was wrapped across two lines reads correctly again.
    let mut out: String = part
        .chars()
        .map(|c| {
            if c.is_control() {
                ' '
            } else if matches!(c, '<' | '>' | '"' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();

    // Runs of spaces left by the above read as a mistake; one space is
    // what the name meant.
    while out.contains("  ") {
        out = out.replace("  ", " ");
    }
    // Windows silently drops trailing dots and spaces, so a name ending
    // in one is a name that would not round-trip.
    let trimmed = out.trim().trim_end_matches(['.', ' ']).trim();
    let mut out = trimmed.to_string();

    if out.chars().count() > LONGEST_PART {
        // Keep the extension, because it is what decides whether the
        // file opens.
        let (stem, ext) = match out.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() && ext.chars().count() <= 10 => {
                (stem.to_string(), Some(ext.to_string()))
            }
            _ => (out.clone(), None),
        };
        let room = match &ext {
            Some(ext) => LONGEST_PART.saturating_sub(ext.chars().count() + 1),
            None => LONGEST_PART,
        };
        let stem: String = stem.chars().take(room).collect();
        let stem = stem.trim_end().to_string();
        out = match ext {
            Some(ext) => format!("{stem}.{ext}"),
            None => stem,
        };
    }

    // A device name is not a name a file may have, whatever follows the
    // dot. Prefixed rather than replaced, so the name is still legible.
    let stem = out.split('.').next().unwrap_or_default();
    if RESERVED.iter().any(|r| stem.eq_ignore_ascii_case(r)) {
        out = format!("_{out}");
    }

    if out.is_empty() || out == "." || out == ".." {
        out = UNNAMED.to_string();
    }
    out
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
        // Refused: every one of these could put a file somewhere other
        // than the directory it was meant for, and no amount of
        // tidying makes that safe. A colon is here rather than with
        // the awkward names because it is a drive or an alternate data
        // stream, not a character somebody typed by accident.
        for bad in [
            "../etc/passwd",
            "a/../../b",
            "/etc/passwd",
            "C:/Windows/x",
            "C:",
            "notes.txt:hidden",
            "a\\b",
            "nul\0byte",
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
        // Cleaned, and the file arrives. These used to be refused
        // alongside the ones above, which is what made a journal PDF
        // with a line break in its name impossible to copy for a week.
        for (awkward, expected) in [
            ("trailing.", "trailing"),
            ("what?", "what_"),
            ("wrapped\nname.pdf", "wrapped name.pdf"),
            ("CON.txt", "_CON.txt"),
        ] {
            let offer = offer_of(&[(awkward, 1, false)]);
            let landing = plan_landing(&offer, &dir, 1 << 30)
                .unwrap_or_else(|e| panic!("{awkward:?} was refused: {e}"));
            assert_eq!(landing.paths, vec![dir.join(expected)]);
            assert_eq!(
                landing.renamed,
                vec![(awkward.to_string(), expected.to_string())],
                "{awkward:?} arrived without saying its name had changed"
            );
        }
        // Harmless oddities are tidied and worth no remark at all.
        let offer = offer_of(&[("./a//b.txt", 1, false)]);
        let landing = plan_landing(&offer, &dir, 1 << 30).unwrap();
        assert_eq!(landing.paths, vec![dir.join("a").join("b.txt")]);
        assert_eq!(landing.top_level, vec![dir.join("a")]);
        assert!(landing.renamed.is_empty());
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

#[cfg(test)]
mod name_tests {
    use super::{usable_parts, LONGEST_PART, UNNAMED};

    fn parts(name: &str) -> Vec<String> {
        usable_parts(name).expect("this name is usable").parts
    }

    fn only(name: &str) -> String {
        let named = usable_parts(name).expect("this name is usable");
        assert_eq!(named.parts.len(), 1, "{name:?} was split");
        named.parts.into_iter().next().unwrap()
    }

    #[test]
    fn the_paper_that_cost_a_week_arrives() {
        // A real filename, from a real machine, refused every time it
        // was copied for a week -- a journal PDF whose name was wrapped
        // across two lines when it was downloaded. Losing a newline out
        // of a filename is not a loss worth refusing a file for.
        let given = "Oracle-Guided Reinforcement Learning for\nDegradation-Aware \
                     Zero-Overpotential Battery Charging.pdf";
        let named = usable_parts(given).expect("this file should arrive");
        assert_eq!(
            named.parts,
            vec![
                "Oracle-Guided Reinforcement Learning for Degradation-Aware \
                 Zero-Overpotential Battery Charging.pdf"
            ]
        );
        // And the change is reported, so the person can see why what
        // they pasted is not called quite what it was.
        assert_eq!(named.changed.len(), 1);
        assert_eq!(named.changed[0].0, given);
        assert!(named.changed[0].1.ends_with(".pdf"));
    }

    #[test]
    fn every_control_character_becomes_the_space_it_was_standing_for() {
        assert_eq!(only("a\tb.txt"), "a b.txt");
        assert_eq!(only("a\r\nb.txt"), "a b.txt");
        assert_eq!(only("a\u{7}b.txt"), "a b.txt");
        // A name that is nothing but control characters still has to be
        // something.
        assert_eq!(only("\n\r\t"), UNNAMED);
    }

    #[test]
    fn a_name_that_is_only_dots_becomes_a_name() {
        // Not an escape -- `...` is an ordinary component -- but
        // Windows drops trailing dots, so it would not round-trip.
        assert_eq!(only("..."), UNNAMED);
        assert_eq!(only("....."), UNNAMED);
        assert_eq!(only("report..."), "report");
        assert_eq!(only("report. . ."), "report");
        // A dot in the middle is an extension and must survive.
        assert_eq!(only("report.v2.pdf"), "report.v2.pdf");
        // A leading dot is a perfectly ordinary hidden file.
        assert_eq!(only(".bashrc"), ".bashrc");
    }

    #[test]
    fn a_reserved_device_name_is_made_into_something_writable() {
        assert_eq!(only("CON"), "_CON");
        assert_eq!(only("con.txt"), "_con.txt");
        assert_eq!(only("COM1.pdf"), "_COM1.pdf");
        assert_eq!(only("LPT9"), "_LPT9");
        assert_eq!(only("nul"), "_nul");
        // Not reserved, and must not be touched.
        assert_eq!(only("CONTENTS.txt"), "CONTENTS.txt");
        assert_eq!(only("COM10.txt"), "COM10.txt");
        assert_eq!(only("console.log"), "console.log");
    }

    #[test]
    fn a_name_too_long_is_shortened_and_keeps_what_opens_it() {
        let long = format!("{}.pdf", "x".repeat(400));
        let used = only(&long);
        assert!(used.chars().count() <= LONGEST_PART, "{}", used.len());
        assert!(
            used.ends_with(".pdf"),
            "the extension decides whether it opens"
        );
        // Exactly at the limit, nothing is touched.
        let exact: String = "y".repeat(LONGEST_PART);
        assert_eq!(only(&exact), exact);
        assert!(usable_parts(&exact).unwrap().changed.is_empty());
    }

    #[test]
    fn the_characters_windows_reserves_are_replaced_rather_than_refused() {
        assert_eq!(only("a<b>c\"d|e?f*g.txt"), "a_b_c_d_e_f_g.txt");
        assert_eq!(
            parts("2026-09-29/notes.txt"),
            vec!["2026-09-29", "notes.txt"]
        );
    }

    #[test]
    fn nothing_that_could_escape_is_ever_cleaned_into_something_usable() {
        // The whole point of separating the two kinds. These are not
        // awkward names, they are names that might not land where they
        // were meant to, and no amount of tidying makes them safe.
        for escape in [
            "../secrets",
            "a/../../b",
            "..",
            "/etc/passwd",
            "C:\\Windows\\System32\\x",
            "a\\b",
            "a\0b",
            "..\\..\\x",
            // A colon is not an awkward character to be tidied up: it
            // means a drive or an alternate data stream. Cleaning it
            // would make `C:` into `C_` and let it through.
            "C:/Windows/x",
            "C:",
            "notes.txt:hidden",
        ] {
            assert!(
                usable_parts(escape).is_none(),
                "{escape:?} was accepted, and it must never be"
            );
        }
    }

    #[test]
    fn cleaning_can_never_produce_a_separator_or_a_dot_component() {
        // A cleaned part that came out as `..` or with a `/` in it
        // would turn a safe name into an escape, which is the one way
        // this change could make things worse rather than better.
        for awkward in [
            "..\u{7}", "a\nb", "...", "  ", "\u{0}x", ". .", "<>", "?", "*", "CON",
        ] {
            let Some(named) = usable_parts(awkward) else {
                continue;
            };
            for part in &named.parts {
                assert!(!part.is_empty(), "{awkward:?} produced an empty part");
                assert!(part != "." && part != "..", "{awkward:?} produced {part:?}");
                assert!(!part.contains('/'), "{awkward:?} produced {part:?}");
                assert!(!part.contains('\\'), "{awkward:?} produced {part:?}");
                assert!(!part.contains('\0'), "{awkward:?} produced {part:?}");
                assert!(
                    !part.ends_with('.') && !part.ends_with(' '),
                    "{awkward:?} produced {part:?}"
                );
            }
        }
    }

    #[test]
    fn a_name_that_needs_nothing_doing_to_it_is_reported_as_unchanged() {
        let named = usable_parts("folder/paper.pdf").expect("usable");
        assert_eq!(named.parts, vec!["folder", "paper.pdf"]);
        assert!(named.changed.is_empty());
        assert_eq!(parts("a/b/c.txt"), vec!["a", "b", "c.txt"]);
    }

    #[test]
    fn a_change_deep_in_a_path_is_still_reported() {
        let named = usable_parts("papers/Oracle\nGuided.pdf").expect("usable");
        assert_eq!(named.parts, vec!["papers", "Oracle Guided.pdf"]);
        assert_eq!(named.changed.len(), 1);
        assert_eq!(named.changed[0].1, "Oracle Guided.pdf");
    }
}
