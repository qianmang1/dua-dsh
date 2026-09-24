//! Structured, streaming traversal output as newline-delimited JSON.
//!
//! This module implements the `--jsonl` output mode of the `aggregate` sub-command. It walks the
//! given inputs with the same [`WalkOptions`] as the human-readable paths, but writes one JSON
//! object per line to stdout instead of rendering a tree or table.
//!
//! Every line is a single event, in this order of first appearance:
//!
//! - `hello` - the first line, carrying the protocol version and walk configuration.
//! - `entry` - one line per traversed entry (including each input root at depth `0`), with its
//!   path, depth, kind, apparent size (`len`) and allocated size (`allocated`).
//! - `error` - one line per filesystem error. Traversal errors reported by `dua-core` carry no
//!   failing path, so the line identifies only the input root that owns the error; the failing
//!   path is never fabricated.
//! - `summary` - aggregate counters and sizes over all successfully emitted entries.
//! - `done` - terminal event with the exit code and overall status.
//!
//! Lines are flushed after every event, so a consumer can process the stream incrementally and
//! exert back-pressure on the traversal through the underlying channel.

use crate::{
    WalkOptions, WalkResult, InodeFilter, crossdev, walk,
    walk::RootEvent,
};
use anyhow::{Context, Result};
use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Allocate `0` bytes for directories to match the CLI's disk-usage accounting, which only sizes
/// files; every other entry reports its filesystem allocation.
#[cfg(any(windows, target_os = "macos"))]
fn allocated_size(entry: &walk::Entry, metadata: &walk::Metadata) -> u64 {
    if entry.file_type.is_dir() {
        0
    } else {
        metadata.allocated_size()
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn allocated_size(entry: &walk::Entry, metadata: &walk::Metadata) -> u64 {
    use filesize::PathExt;
    if entry.file_type.is_dir() {
        0
    } else {
        entry.path().size_on_disk_fast(metadata).unwrap_or(0)
    }
}

/// Return the protocol-facing kind of a traversed entry.
fn entry_kind(entry: &walk::Entry) -> &'static str {
    if entry.file_type.is_dir() {
        "dir"
    } else {
        "file"
    }
}

/// Append a JSON string literal for `text` to `out`.
///
/// Paths and messages come from the operating system and may contain control characters, so they
/// are escaped rather than passed through.
fn push_json_string(out: &mut String, text: &str) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character if character.is_control() => {
                out.push('\\');
                out.push('u');
                write!(out, "{:04x}", character as u32).expect("fmt cannot fail");
            }
            character => out.push(character),
        }
    }
    out.push('"');
}

/// Accumulated totals over all entries emitted for the current scan.
#[derive(Default)]
struct Totals {
    entries: u64,
    dirs: u64,
    files: u64,
    errors: u64,
    apparent_bytes: u64,
    allocated_bytes: u64,
}

/// Walk `paths` and stream one NDJSON event per line to `out`.
///
/// Every input in `paths` becomes a root at depth `0`, mirroring the `aggregate --depth` input
/// model: a directory input is reported as one entry whose children follow at depth `1`. All
/// depths are streamed; retention and depth limiting belong to the consumer. Traversal stops
/// early if writing fails, returning the error to the caller.
pub fn aggregate_jsonl(
    mut out: impl Write,
    walk_options: WalkOptions,
    paths: Vec<PathBuf>,
) -> Result<WalkResult> {
    let mut totals = Totals::default();
    write_hello(&mut out, &walk_options, paths.len())?;

    // Each root keeps its normalized path and device id; roots whose device cannot be determined
    // are reported as errors and skipped, mirroring the flat aggregate path.
    let cross_filesystems = walk_options.cross_filesystems;
    let ignore_patterns = walk_options.ignore_patterns.is_some();
    let mut roots: Vec<crate::common::WalkRoot> = Vec::with_capacity(paths.len());
    let mut root_paths: Vec<PathBuf> = Vec::with_capacity(paths.len());
    let mut root_emitted = Vec::with_capacity(paths.len());
    let mut device_ids: Vec<u64> = Vec::with_capacity(paths.len());
    for (index, path) in paths.into_iter().enumerate() {
        let device_id = if cross_filesystems {
            0
        } else {
            match crossdev::init(&path) {
                Ok(device_id) => device_id,
                Err(err) => {
                    write_error(
                        &mut out,
                        Some(&path),
                        Some(path.as_path()),
                        "root",
                        &err.to_string(),
                        &mut totals,
                    )?;
                    continue;
                }
            }
        };
        root_paths.push(path.clone());
        root_emitted.push(false);
        device_ids.push(device_id);
        roots.push(crate::common::WalkRoot {
            index,
            pattern_root: ignore_patterns.then(|| path.clone()),
            path,
            #[cfg(any(windows, target_os = "macos"))]
            entry: None,
            device_id,
        });
    }

    let mut inodes = InodeFilter::default();
    for (root_idx, event) in
        walk_options.iter_from_paths(roots, false, walk::Order::ParentFirst)
    {
        match event {
            RootEvent::Finished => {}
            RootEvent::Entry(Ok(entry)) => {
                if entry.depth == 0 && let Some(flag) = root_emitted.get_mut(root_idx) {
                    *flag = true;
                }
                totals.entries += 1;
                if entry.file_type.is_dir() {
                    totals.dirs += 1;
                } else {
                    totals.files += 1;
                }
                let root = root_paths.get(root_idx);
                match &entry.metadata {
                    Some(Ok(metadata)) => {
                        // Mirror the flat aggregate's hard-link accounting: the first
                        // observation of each (volume, file) identity is counted, and
                        // later links of the same identity contribute zero bytes unless
                        // `--count-hard-links` is set. Directories never collide, so they
                        // always pass.
                        let counted = walk_options.count_hard_links
                            || inodes.add(&entry, metadata)
                                && (walk_options.cross_filesystems
                                    || crossdev::is_same_device(
                                        device_ids.get(root_idx).copied().unwrap_or(0),
                                        metadata,
                                    ));
                        let len = if counted { metadata.len() } else { 0 };
                        let allocated = if counted { allocated_size(&entry, metadata) } else { 0 };
                        totals.apparent_bytes = totals.apparent_bytes.saturating_add(len);
                        totals.allocated_bytes = totals.allocated_bytes.saturating_add(allocated);
                        write_entry(&mut out, &entry, entry_kind(&entry), len, allocated)?;
                    }
                    Some(Err(err)) => {
                        // The failing entry is known here (metadata is read per entry), so its
                        // exact path is reported rather than fabricated.
                        let path = entry.path();
                        write_error(
                            &mut out,
                            root,
                            Some(path.as_path()),
                            "metadata",
                            &err.to_string(),
                            &mut totals,
                        )?;
                    }
                    None => {
                        // `Options::skip_metadata` is never set by the CLI, so this cannot occur
                        // in practice; emit a zero-sized entry rather than guessing a size.
                        write_entry(&mut out, &entry, entry_kind(&entry), 0, 0)?;
                    }
                }
            }
            RootEvent::Entry(Err(err)) => {
                let root = root_paths.get(root_idx);
                // A root whose own preparation failed never emitted a depth-`0` entry, so the
                // error is attributed to the input path exactly. Errors that arrive after the
                // root was reported cannot be tied to a directory by `dua-core`, which yields
                // them without any path; only the owning input root is named.
                let (kind, exact_path) = if root_emitted.get(root_idx).copied() == Some(false) {
                    ("root", root.cloned())
                } else {
                    ("read", None)
                };
                write_error(
                    &mut out,
                    root,
                    exact_path.as_deref(),
                    kind,
                    &err.to_string(),
                    &mut totals,
                )?;
            }
        }
    }

    write_summary(&mut out, &totals)?;
    write_done(&mut out, &totals)?;
    Ok(WalkResult {
        num_errors: totals.errors,
    })
}

fn write_hello(out: &mut impl Write, walk_options: &WalkOptions, roots: usize) -> Result<()> {
    let mut line = String::from("{\"protocol\":1,\"type\":\"hello\",\"version\":");
    push_json_string(&mut line, env!("CARGO_PKG_VERSION"));
    line.push_str(&format!(
        ",\"threads\":{},\"roots\":{},\"order\":\"parent-first\"}}",
        walk_options.threads, roots
    ));
    write_flushed(out, &line)
}

fn write_entry(
    out: &mut impl Write,
    entry: &walk::Entry,
    kind: &str,
    len: u64,
    allocated: u64,
) -> Result<()> {
    let mut line = String::from("{\"protocol\":1,\"type\":\"entry\",\"path\":");
    push_json_string(&mut line, &entry.path().to_string_lossy());
    line.push_str(&format!(
        ",\"depth\":{},\"kind\":",
        entry.depth
    ));
    push_json_string(&mut line, kind);
    line.push_str(&format!(",\"len\":{len},\"allocated\":{allocated}}}"));
    write_flushed(out, &line)
}

/// `exact_path` is `Some` only when the failing path is genuinely known; the design contract is
/// that a failing path is never fabricated when `dua-core` does not report one.
fn write_error(
    out: &mut impl Write,
    root: Option<&PathBuf>,
    exact_path: Option<&Path>,
    kind: &str,
    message: &str,
    totals: &mut Totals,
) -> Result<()> {
    totals.errors += 1;
    let mut line = String::from("{\"protocol\":1,\"type\":\"error\",\"root\":");
    match root {
        Some(path) => push_json_string(&mut line, &path.to_string_lossy()),
        None => line.push_str("null"),
    }
    line.push_str(",\"parent\":");
    match exact_path.and_then(Path::parent) {
        Some(parent) => push_json_string(&mut line, &parent.to_string_lossy()),
        None => line.push_str("null"),
    }
    line.push_str(",\"path\":");
    match exact_path {
        Some(path) => push_json_string(&mut line, &path.to_string_lossy()),
        None => line.push_str("null"),
    }
    line.push_str(",\"kind\":");
    push_json_string(&mut line, kind);
    line.push_str(",\"message\":");
    push_json_string(&mut line, message);
    line.push('}');
    write_flushed(out, &line)
}

fn write_summary(out: &mut impl Write, totals: &Totals) -> Result<()> {
    let line = format!(
        "{{\"protocol\":1,\"type\":\"summary\",\"entries\":{},\"errors\":{},\"dirs\":{},\"files\":{},\"apparentBytes\":{},\"allocatedBytes\":{}}}",
        totals.entries,
        totals.errors,
        totals.dirs,
        totals.files,
        totals.apparent_bytes,
        totals.allocated_bytes
    );
    write_flushed(out, &line)
}

fn write_done(out: &mut impl Write, totals: &Totals) -> Result<()> {
    let (ok, reason, exit_code) = if totals.errors == 0 {
        ("true", "completed", 0)
    } else {
        ("false", "partial", 1)
    };
    let line = format!(
        "{{\"protocol\":1,\"type\":\"done\",\"exitCode\":{exit_code},\"ok\":{ok},\"reason\":\"{reason}\"}}"
    );
    write_flushed(out, &line)
}

fn write_flushed(out: &mut impl Write, line: &str) -> Result<()> {
    writeln!(out, "{line}").context("could not write NDJSON event")?;
    out.flush().context("could not flush NDJSON event")
}