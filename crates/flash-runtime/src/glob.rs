//! Platform-neutral parsing and traversal for explicit filesystem globbing.
//!
//! The runtime owns pattern meaning and deterministic ordering. Host access is
//! injected as one directory-batch callback, so matching never reaches around
//! the `Platform::read_directory` boundary.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use flash_platform::{DirectoryEntry, DirectoryEntryKind};

/// Maximum directory entries one explicit glob may inspect before completing.
pub(crate) const DEFAULT_GLOB_ENTRY_LIMIT: usize = 1_000_000;

/// Matches one complete native path with the same component semantics as `glob`.
///
/// This performs no filesystem access. Callers that supply path candidates are
/// responsible for ensuring recursive components represent traversable,
/// non-symlink directory structure.
pub fn glob_pattern_matches(pattern: &OsStr, candidate: &OsStr) -> Result<bool, GlobPatternError> {
    Ok(GlobPattern::parse(pattern)?.matches(candidate))
}

/// One validated glob pattern ready for deterministic traversal.
pub(crate) struct GlobPattern {
    initial: PathBuf,
    segments: Vec<PatternSegment>,
    empty: bool,
}

impl GlobPattern {
    /// Parses native path units without converting invalid names to UTF-8.
    pub(crate) fn parse(pattern: &OsStr) -> Result<Self, GlobPatternError> {
        if pattern.is_empty() {
            return Ok(Self {
                initial: PathBuf::new(),
                segments: Vec::new(),
                empty: true,
            });
        }
        if pattern.as_bytes().contains(&0) {
            return Err(GlobPatternError::new("patterns cannot contain NUL"));
        }

        let mut initial = PathBuf::new();
        let mut segments = Vec::new();
        let mut saw_curdir = false;
        for component in Path::new(pattern).components() {
            match component {
                Component::Prefix(prefix) => initial.push(prefix.as_os_str()),
                Component::RootDir => initial.push(Path::new("/")),
                Component::CurDir => saw_curdir = true,
                Component::ParentDir => segments.push(PatternSegment::Parent),
                Component::Normal(component) => {
                    segments.push(PatternSegment::parse(component)?);
                }
            }
        }
        if saw_curdir && initial.as_os_str().is_empty() && segments.is_empty() {
            initial.push(".");
        }

        Ok(Self {
            initial,
            segments,
            empty: false,
        })
    }

    /// Expands through a logical-cwd-relative directory reader.
    pub(crate) fn expand<E>(
        &self,
        mut read_directory: impl FnMut(&Path) -> Result<Vec<DirectoryEntry>, E>,
    ) -> Result<Vec<PathBuf>, E> {
        if self.empty {
            return Ok(Vec::new());
        }

        let mut paths = vec![self.initial.clone()];
        for (index, segment) in self.segments.iter().enumerate() {
            let final_segment = index + 1 == self.segments.len();
            paths = match segment {
                PatternSegment::Parent => paths
                    .into_iter()
                    .map(|mut path| {
                        path.push("..");
                        path
                    })
                    .collect(),
                PatternSegment::Recursive => {
                    let mut directories = BTreeSet::new();
                    for path in paths {
                        collect_recursive_directories(path, &mut directories, &mut read_directory)?;
                    }
                    directories.into_iter().collect()
                }
                PatternSegment::Component(pattern) => {
                    let mut matched = Vec::new();
                    for directory in paths {
                        let read_path = if directory.as_os_str().is_empty() {
                            Path::new(".")
                        } else {
                            directory.as_path()
                        };
                        let mut entries = read_directory(read_path)?;
                        entries.sort_by(|left, right| left.name().cmp(right.name()));
                        for entry in entries {
                            if is_dot_entry(entry.name()) || !pattern.matches(entry.name()) {
                                continue;
                            }
                            if !final_segment && entry.kind() != DirectoryEntryKind::Directory {
                                continue;
                            }
                            let mut path = directory.clone();
                            path.push(entry.name());
                            matched.push(path);
                        }
                    }
                    matched
                }
            };
        }

        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    fn matches(&self, candidate: &OsStr) -> bool {
        if self.empty {
            return candidate.is_empty();
        }
        let candidate_path = Path::new(candidate);
        if self.initial.is_absolute() != candidate_path.is_absolute() {
            return false;
        }
        let candidate_segments: Vec<_> = candidate_path
            .components()
            .filter_map(|component| match component {
                Component::Normal(value) => Some(value),
                Component::ParentDir => Some(OsStr::new("..")),
                Component::CurDir | Component::RootDir | Component::Prefix(_) => None,
            })
            .collect();
        match_segments(&self.segments, &candidate_segments)
    }
}

fn match_segments(pattern: &[PatternSegment], candidate: &[&OsStr]) -> bool {
    let Some((head, rest)) = pattern.split_first() else {
        return candidate.is_empty();
    };
    match head {
        PatternSegment::Parent => {
            candidate.first() == Some(&OsStr::new("..")) && match_segments(rest, &candidate[1..])
        }
        PatternSegment::Recursive => {
            match_segments(rest, candidate)
                || candidate.first().is_some_and(|name| {
                    !starts_with_dot(name) && match_segments(pattern, &candidate[1..])
                })
        }
        PatternSegment::Component(component) => candidate
            .first()
            .is_some_and(|name| component.matches(name) && match_segments(rest, &candidate[1..])),
    }
}

fn collect_recursive_directories<E>(
    root: PathBuf,
    directories: &mut BTreeSet<PathBuf>,
    read_directory: &mut impl FnMut(&Path) -> Result<Vec<DirectoryEntry>, E>,
) -> Result<(), E> {
    let mut pending = vec![root];
    while let Some(directory) = pending.pop() {
        if !directories.insert(directory.clone()) {
            continue;
        }
        let read_path = if directory.as_os_str().is_empty() {
            Path::new(".")
        } else {
            directory.as_path()
        };
        let mut entries = read_directory(read_path)?;
        entries.sort_by(|left, right| left.name().cmp(right.name()));
        let mut children = Vec::new();
        for entry in entries {
            if entry.kind() != DirectoryEntryKind::Directory
                || is_dot_entry(entry.name())
                || starts_with_dot(entry.name())
            {
                continue;
            }
            let mut child = directory.clone();
            child.push(entry.name());
            children.push(child);
        }
        pending.extend(children.into_iter().rev());
    }
    Ok(())
}

fn is_dot_entry(name: &OsStr) -> bool {
    name == OsStr::new(".") || name == OsStr::new("..")
}

fn starts_with_dot(name: &OsStr) -> bool {
    native_chars(name).first() == Some(&NativeChar::Unicode('.'))
}

enum PatternSegment {
    Parent,
    Recursive,
    Component(ComponentPattern),
}

impl PatternSegment {
    fn parse(component: &OsStr) -> Result<Self, GlobPatternError> {
        let chars = native_chars(component);
        if chars == [NativeChar::Unicode('*'), NativeChar::Unicode('*')] {
            return Ok(Self::Recursive);
        }
        Ok(Self::Component(ComponentPattern::parse(chars)?))
    }
}

struct ComponentPattern {
    atoms: Vec<PatternAtom>,
    explicit_hidden: bool,
}

impl ComponentPattern {
    fn parse(chars: Vec<NativeChar>) -> Result<Self, GlobPatternError> {
        let mut atoms = Vec::new();
        let mut index = 0;
        while index < chars.len() {
            match chars[index] {
                NativeChar::Unicode('\\') => {
                    index += 1;
                    let Some(character) = chars.get(index).copied() else {
                        return Err(GlobPatternError::new(
                            "a trailing escape has no pattern character",
                        ));
                    };
                    atoms.push(PatternAtom::Literal(character));
                    index += 1;
                }
                NativeChar::Unicode('*') => {
                    if !matches!(atoms.last(), Some(PatternAtom::AnyMany)) {
                        atoms.push(PatternAtom::AnyMany);
                    }
                    index += 1;
                }
                NativeChar::Unicode('?') => {
                    atoms.push(PatternAtom::AnyOne);
                    index += 1;
                }
                NativeChar::Unicode('[') => {
                    let (class, next) = CharacterClass::parse(&chars, index + 1)?;
                    atoms.push(PatternAtom::Class(class));
                    index = next;
                }
                character => {
                    atoms.push(PatternAtom::Literal(character));
                    index += 1;
                }
            }
        }

        let explicit_hidden = matches!(
            atoms.first(),
            Some(PatternAtom::Literal(NativeChar::Unicode('.')))
        );
        Ok(Self {
            atoms,
            explicit_hidden,
        })
    }

    fn matches(&self, name: &OsStr) -> bool {
        if starts_with_dot(name) && !self.explicit_hidden {
            return false;
        }
        let chars = native_chars(name);
        let mut current = vec![false; chars.len() + 1];
        current[0] = true;
        for atom in &self.atoms {
            let mut next = vec![false; chars.len() + 1];
            match atom {
                PatternAtom::AnyMany => {
                    next[0] = current[0];
                    for index in 1..=chars.len() {
                        next[index] = current[index] || next[index - 1];
                    }
                }
                PatternAtom::AnyOne => {
                    for index in 0..chars.len() {
                        next[index + 1] |= current[index];
                    }
                }
                PatternAtom::Literal(expected) => {
                    for (index, actual) in chars.iter().enumerate() {
                        next[index + 1] |= current[index] && actual == expected;
                    }
                }
                PatternAtom::Class(class) => {
                    for (index, actual) in chars.iter().enumerate() {
                        next[index + 1] |= current[index] && class.matches(*actual);
                    }
                }
            }
            current = next;
        }
        current[chars.len()]
    }
}

enum PatternAtom {
    Literal(NativeChar),
    AnyOne,
    AnyMany,
    Class(CharacterClass),
}

struct CharacterClass {
    negated: bool,
    ranges: Vec<(NativeChar, NativeChar)>,
}

impl CharacterClass {
    fn parse(pattern: &[NativeChar], mut index: usize) -> Result<(Self, usize), GlobPatternError> {
        let mut negated = false;
        if matches!(
            pattern.get(index),
            Some(NativeChar::Unicode('!') | NativeChar::Unicode('^'))
        ) {
            negated = true;
            index += 1;
        }

        let mut items = Vec::new();
        let mut closed = false;
        while index < pattern.len() {
            match pattern[index] {
                NativeChar::Unicode(']') => {
                    closed = true;
                    index += 1;
                    break;
                }
                NativeChar::Unicode('\\') => {
                    index += 1;
                    let Some(character) = pattern.get(index).copied() else {
                        return Err(GlobPatternError::new(
                            "a character-class escape has no following character",
                        ));
                    };
                    items.push((character, true));
                    index += 1;
                }
                character => {
                    items.push((character, false));
                    index += 1;
                }
            }
        }
        if !closed {
            return Err(GlobPatternError::new("a character class is missing `]`"));
        }
        if items.is_empty() {
            return Err(GlobPatternError::new("a character class cannot be empty"));
        }

        let mut ranges = Vec::new();
        let mut item = 0;
        while item < items.len() {
            let start = items[item].0;
            if item + 2 < items.len() && items[item + 1] == (NativeChar::Unicode('-'), false) {
                let end = items[item + 2].0;
                if native_order(start) > native_order(end) {
                    return Err(GlobPatternError::new(
                        "a character-class range cannot descend",
                    ));
                }
                ranges.push((start, end));
                item += 3;
            } else {
                ranges.push((start, start));
                item += 1;
            }
        }
        Ok((Self { negated, ranges }, index))
    }

    fn matches(&self, character: NativeChar) -> bool {
        let value = native_order(character);
        let contained = self
            .ranges
            .iter()
            .any(|(start, end)| native_order(*start) <= value && value <= native_order(*end));
        contained != self.negated
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeChar {
    Unicode(char),
    Invalid(u8),
}

fn native_order(character: NativeChar) -> u32 {
    match character {
        NativeChar::Unicode(character) => u32::from(character),
        NativeChar::Invalid(byte) => 0x11_0000 + u32::from(byte),
    }
}

fn native_chars(value: &OsStr) -> Vec<NativeChar> {
    let mut bytes = value.as_bytes();
    let mut chars = Vec::new();
    while !bytes.is_empty() {
        match std::str::from_utf8(bytes) {
            Ok(text) => {
                chars.extend(text.chars().map(NativeChar::Unicode));
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                let text = std::str::from_utf8(&bytes[..valid])
                    .expect("the UTF-8 validator identifies a valid prefix");
                chars.extend(text.chars().map(NativeChar::Unicode));
                bytes = &bytes[valid..];
                let invalid = error.error_len().unwrap_or(1).min(bytes.len());
                chars.extend(bytes[..invalid].iter().copied().map(NativeChar::Invalid));
                bytes = &bytes[invalid..];
            }
        }
    }
    chars
}

/// A deterministic validation error raised before directory access.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GlobPatternError {
    message: String,
}

impl GlobPatternError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}
