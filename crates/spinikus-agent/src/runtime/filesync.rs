//! The build context over the BuildKit session: BuildKit's `FileSync.DiffCopy`, which
//! frontends that read local sources (`llb.Local("context")`, `llb.Local("dockerfile")`)
//! need, such as Railpack's. An uploaded tarball doesn't give them any, so their builds fail
//! with "no local sources enabled".
//!
//! The protocol is fsutil's: the agent sends a `STAT` packet for every entry it shares, in
//! walk order (each directory before what it holds, names in byte order within a
//! directory), then an empty `STAT`. BuildKit asks for the regular files it lacks with `REQ`
//! packets naming an entry by its index, and gets each as `DATA` packets ending with an
//! empty one. It sends `FIN` when it has everything, and the agent answers with `FIN`.
//!
//! Which entries are shared comes from the call's metadata: `dir-name` (`context` and
//! `dockerfile` are both the context directory), `followpaths` and `include-patterns` (only
//! those paths, with their parents), and `exclude-patterns` (`.dockerignore`-style globs,
//! `!` re-including). `.git` is never shared. Owners are sent as root, as a tarball would
//! have them; modes are Go's `os.FileMode`, which fsutil uses.

use std::{
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    pin::Pin,
};

use bollard_buildkit_proto::{
    fsutil::types::{Packet, Stat, packet::PacketType},
    moby::filesync::v1::file_sync_server::FileSync,
};
use tokio::{io::AsyncReadExt, sync::mpsc};
use tokio_stream::wrappers::ReceiverStream;

/// How much of a file each `DATA` packet carries.
const CHUNK: usize = 32 * 1024;

/// Go's `os.FileMode` type bits.
const GO_MODE_DIR: u32 = 1 << 31;
const GO_MODE_SYMLINK: u32 = 1 << 27;
const GO_MODE_SETUID: u32 = 1 << 23;
const GO_MODE_SETGID: u32 = 1 << 22;
const GO_MODE_STICKY: u32 = 1 << 20;

/// Shares one directory as the build's local sources.
pub(super) struct LocalSources {
    pub(super) root: PathBuf,
}

#[tonic::async_trait]
impl FileSync for LocalSources {
    type DiffCopyStream = ReceiverStream<Result<Packet, tonic::Status>>;

    async fn diff_copy(
        &self,
        request: tonic::Request<tonic::Streaming<Packet>>,
    ) -> Result<tonic::Response<Self::DiffCopyStream>, tonic::Status> {
        let metadata = request.metadata();
        let values = |key: &str| -> Vec<String> {
            metadata.get_all(key).iter().filter_map(|value| value.to_str().ok()).map(str::to_string).collect()
        };
        let name = values("dir-name").into_iter().next().unwrap_or_default();
        if !matches!(name.as_str(), "context" | "dockerfile") {
            return Err(tonic::Status::not_found(format!("there is no local source `{name}`")));
        }
        let filter = Filter::new(&values("include-patterns"), &values("followpaths"), &values("exclude-patterns"));
        let root = self.root.clone();
        let entries = tokio::task::spawn_blocking(move || walk(&root, &filter))
            .await
            .map_err(|error| tonic::Status::internal(error.to_string()))?
            .map_err(|error| tonic::Status::internal(format!("reading the build context: {error}")))?;
        tracing::debug!(source = %name, entries = entries.len(), "sharing a local source with BuildKit");

        let (sender, receiver) = mpsc::channel(64);
        let paths: Vec<Option<PathBuf>> = entries.iter().map(|entry| entry.file.clone()).collect();
        let stats = sender.clone();
        tokio::spawn(async move {
            for entry in entries {
                let packet = Packet { r#type: PacketType::PacketStat.into(), stat: Some(entry.stat), ..Default::default() };
                if stats.send(Ok(packet)).await.is_err() {
                    return;
                }
            }
            let _ = stats.send(Ok(Packet { r#type: PacketType::PacketStat.into(), ..Default::default() })).await;
        });
        let mut incoming = request.into_inner();
        tokio::spawn(async move {
            loop {
                let packet = match incoming.message().await {
                    Ok(Some(packet)) => packet,
                    Ok(None) => return,
                    Err(error) => {
                        tracing::debug!(%error, "BuildKit closed a local source");
                        return;
                    }
                };
                match PacketType::try_from(packet.r#type) {
                    Ok(PacketType::PacketReq) => {
                        let path = paths.get(packet.id as usize).cloned().flatten();
                        if let Err(error) = send_file(&sender, packet.id, path.as_deref()).await {
                            let data = error.into_bytes();
                            let _ = sender
                                .send(Ok(Packet { r#type: PacketType::PacketErr.into(), data, ..Default::default() }))
                                .await;
                            return;
                        }
                    }
                    Ok(PacketType::PacketFin) => {
                        let _ = sender.send(Ok(Packet { r#type: PacketType::PacketFin.into(), ..Default::default() })).await;
                        return;
                    }
                    Ok(PacketType::PacketErr) => {
                        tracing::debug!(error = %String::from_utf8_lossy(&packet.data), "BuildKit failed to copy a local source");
                        return;
                    }
                    _ => {}
                }
            }
        });
        Ok(tonic::Response::new(ReceiverStream::new(receiver)))
    }

    type TarStreamStream = Pin<Box<dyn tokio_stream::Stream<Item = Result<Packet, tonic::Status>> + Send>>;

    async fn tar_stream(
        &self,
        _request: tonic::Request<tonic::Streaming<Packet>>,
    ) -> Result<tonic::Response<Self::TarStreamStream>, tonic::Status> {
        Err(tonic::Status::unimplemented("local sources are shared with DiffCopy"))
    }
}

/// Sends the file of entry `id` as `DATA` packets, then the empty one that ends it.
async fn send_file(sender: &mpsc::Sender<Result<Packet, tonic::Status>>, id: u32, path: Option<&Path>) -> Result<(), String> {
    let path = path.ok_or_else(|| format!("entry {id} is not a file that was shared"))?;
    let mut file = tokio::fs::File::open(path).await.map_err(|error| format!("opening {}: {error}", path.display()))?;
    let mut buffer = vec![0; CHUNK];
    loop {
        let read = file.read(&mut buffer).await.map_err(|error| format!("reading {}: {error}", path.display()))?;
        let data = buffer[..read].to_vec();
        let last = read == 0;
        let packet = Packet { r#type: PacketType::PacketData.into(), id, data, ..Default::default() };
        if sender.send(Ok(packet)).await.is_err() || last {
            return Ok(());
        }
    }
}

/// An entry to share: its stat, and the file to read if it is a regular file.
#[derive(Debug)]
struct Entry {
    stat: Stat,
    file: Option<PathBuf>,
}

/// Which paths (relative, `/`-separated) are shared.
#[derive(Debug, Default)]
struct Filter {
    /// Only these and what is under them, if any.
    includes: Vec<Pattern>,
    excludes: Vec<(Pattern, bool)>,
}

impl Filter {
    fn new(includes: &[String], follow: &[String], excludes: &[String]) -> Self {
        let includes = includes.iter().chain(follow).filter_map(|pattern| Pattern::new(pattern)).collect();
        let excludes = excludes
            .iter()
            .filter_map(|pattern| match pattern.strip_prefix('!') {
                Some(negated) => Pattern::new(negated).map(|pattern| (pattern, true)),
                None => Pattern::new(pattern).map(|pattern| (pattern, false)),
            })
            .collect();
        Self { includes, excludes }
    }

    fn included(&self, path: &str) -> bool {
        self.includes.is_empty() || self.includes.iter().any(|pattern| pattern.matches_or_parent(path))
    }

    /// The last pattern that matches decides, as in `.dockerignore`.
    fn excluded(&self, path: &str) -> bool {
        let mut excluded = false;
        for (pattern, negated) in &self.excludes {
            if pattern.matches_or_parent(path) {
                excluded = !negated;
            }
        }
        excluded
    }

    /// Whether anything under an excluded directory could be shared again.
    fn reincludes(&self) -> bool {
        self.excludes.iter().any(|(_, negated)| *negated)
    }

    /// Whether something under `dir` could be included.
    fn may_include_under(&self, dir: &str) -> bool {
        self.includes.is_empty() || self.includes.iter().any(|pattern| pattern.may_match_under(dir))
    }
}

/// A `/`-separated glob: `*`, `?` and `[…]` within a part, `**` across parts.
#[derive(Debug)]
struct Pattern(Vec<String>);

impl Pattern {
    fn new(pattern: &str) -> Option<Self> {
        let parts: Vec<String> =
            pattern.trim().split('/').filter(|part| !part.is_empty() && *part != ".").map(str::to_string).collect();
        (!parts.is_empty()).then_some(Self(parts))
    }

    /// Whether the pattern matches `path` or one of its parent directories.
    fn matches_or_parent(&self, path: &str) -> bool {
        let parts: Vec<&str> = path.split('/').collect();
        (1..=parts.len()).any(|len| match_parts(&self.0, &parts[..len]))
    }

    /// Whether a path under `dir` could match.
    fn may_match_under(&self, dir: &str) -> bool {
        let parts: Vec<&str> = dir.split('/').collect();
        prefix_may_match(&self.0, &parts)
    }
}

fn match_parts(pattern: &[String], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((first, rest)) if first == "**" => (0..=path.len()).any(|skip| match_parts(rest, &path[skip..])),
        Some((first, rest)) => match path.split_first() {
            Some((part, path_rest)) => match_part(first.as_bytes(), part.as_bytes()) && match_parts(rest, path_rest),
            None => false,
        },
    }
}

/// Whether some path starting with `dir` could match `pattern`.
fn prefix_may_match(pattern: &[String], dir: &[&str]) -> bool {
    match (pattern.split_first(), dir.split_first()) {
        (_, None) => true,
        (None, Some(_)) => false,
        (Some((first, _)), Some(_)) if first == "**" => true,
        (Some((first, rest)), Some((part, dir_rest))) => {
            match_part(first.as_bytes(), part.as_bytes()) && prefix_may_match(rest, dir_rest)
        }
    }
}

/// Go's `filepath.Match` for one path part.
fn match_part(pattern: &[u8], name: &[u8]) -> bool {
    match pattern.split_first() {
        None => name.is_empty(),
        Some((b'*', rest)) => (0..=name.len()).any(|skip| match_part(rest, &name[skip..])),
        Some((b'?', rest)) => !name.is_empty() && match_part(rest, &name[1..]),
        Some((b'[', rest)) => match (name.split_first(), class(rest)) {
            (Some((byte, name_rest)), Some((matches, after))) => matches(*byte) && match_part(after, name_rest),
            _ => false,
        },
        Some((b'\\', rest)) if !rest.is_empty() => name.first() == Some(&rest[0]) && match_part(&rest[1..], &name[1..]),
        Some((byte, rest)) => name.first() == Some(byte) && match_part(rest, &name[1..]),
    }
}

/// A `[…]` class after its `[`: a test for one byte, and the pattern after its `]`.
#[allow(clippy::type_complexity)]
fn class(pattern: &[u8]) -> Option<(Box<dyn Fn(u8) -> bool + '_>, &[u8])> {
    let (negated, body) = match pattern.first() {
        Some(b'^' | b'!') => (true, &pattern[1..]),
        _ => (false, pattern),
    };
    let end = body.iter().skip(1).position(|byte| *byte == b']')? + 1;
    let (set, after) = (&body[..end], &body[end + 1..]);
    let test = move |byte: u8| {
        let mut index = 0;
        let mut found = false;
        while index < set.len() {
            if index + 2 < set.len() && set[index + 1] == b'-' {
                found |= (set[index]..=set[index + 2]).contains(&byte);
                index += 3;
            } else {
                found |= set[index] == byte;
                index += 1;
            }
        }
        found != negated
    };
    Some((Box::new(test), after))
}

/// The entries of `root` that `filter` shares, in the order fsutil needs.
fn walk(root: &Path, filter: &Filter) -> std::io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    let mut pending = Vec::new();
    visit(root, "", filter, &mut entries, &mut pending)?;
    Ok(entries)
}

/// Adds the entries of `dir` (at `relative`). Directories are added only when something in
/// them is, or when they are shared themselves: `pending` holds those entered but not yet
/// added, which go in before the first entry under them.
fn visit(dir: &Path, relative: &str, filter: &Filter, entries: &mut Vec<Entry>, pending: &mut Vec<Entry>) -> std::io::Result<()> {
    let mut children = std::fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        let name = child.file_name();
        if name == ".git" {
            continue;
        }
        let Some(name) = name.to_str() else {
            tracing::warn!(path = %child.path().display(), "skipping a file whose name isn't UTF-8");
            continue;
        };
        let path = match relative.is_empty() {
            true => name.to_string(),
            false => format!("{relative}/{name}"),
        };
        let metadata = std::fs::symlink_metadata(child.path())?;
        let excluded = filter.excluded(&path);
        let shared = filter.included(&path) && !excluded;
        let file_type = metadata.file_type();
        if file_type.is_dir() {
            if (excluded && !filter.reincludes()) || !filter.may_include_under(&path) {
                continue;
            }
            let depth = pending.len();
            pending.push(Entry { stat: stat(&path, &metadata, None), file: None });
            if shared {
                entries.append(pending);
            }
            visit(&child.path(), &path, filter, entries, pending)?;
            // Gone already if something in it (or it) was added, with its parents.
            pending.truncate(depth);
        } else if shared && (file_type.is_file() || file_type.is_symlink()) {
            let link = match file_type.is_symlink() {
                true => Some(std::fs::read_link(child.path())?),
                false => None,
            };
            entries.append(pending);
            let file = file_type.is_file().then(|| child.path());
            entries.push(Entry { stat: stat(&path, &metadata, link.as_deref()), file });
        }
    }
    Ok(())
}

fn stat(path: &str, metadata: &std::fs::Metadata, link: Option<&Path>) -> Stat {
    let unix = metadata.permissions().mode();
    let mut mode = unix & 0o777;
    if unix & 0o4000 != 0 {
        mode |= GO_MODE_SETUID;
    }
    if unix & 0o2000 != 0 {
        mode |= GO_MODE_SETGID;
    }
    if unix & 0o1000 != 0 {
        mode |= GO_MODE_STICKY;
    }
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        mode |= GO_MODE_DIR;
    } else if file_type.is_symlink() {
        mode |= GO_MODE_SYMLINK;
    }
    Stat {
        path: path.to_string(),
        mode,
        size: if file_type.is_dir() { 0 } else { metadata.len() as i64 },
        mod_time: metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec(),
        linkname: link.map(|link| link.to_string_lossy().into_owned()).unwrap_or_default(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests;
