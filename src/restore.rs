// restore.rs — the actual undo machinery.
//
// Every restore follows the same two-step dance:
//
//   1. Snapshot the current state of the target file as a `pre-restore` event.
//      This is the safety rule from PHILOSOPHY.md: we never destroy data to
//      recover data. Undo-the-undo is always one command away.
//
//   2. Write (or delete) the target state, and record it as an
//      `agent-undo-restore` event. The file_state table is updated so the
//      daemon's dedup check sees the new state as current and doesn't snapshot
//      the write-back as a spurious user edit.
//
// The oops/session variants are just bulk applications of `restore_file_to`.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::store::{EventRow, NewEvent, Store};

// ═══════════════════════════════════════════════════════════════
// Revert plan types
// ═══════════════════════════════════════════════════════════════

/// Kind of change for a planned revert.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanKind {
    Modified, // both sides have content, content differs
    Created,  // old doesn't exist, new does → revert will delete
    Deleted,  // old exists, new doesn't → revert will restore
}

/// A single file in a revert plan (read-only, for preview).
#[derive(Debug, Clone)]
pub struct PlanItem {
    pub path: String,
    pub kind: PlanKind,
    pub old_content: Vec<u8>,     // target state content
    pub new_content: Vec<u8>,     // current disk content
    pub target_hash: Option<String>, // blob hash of target state
    pub event_id: i64,
    pub attribution: String,
}

/// JSON output structures (modeled after GitHub Commit API).
#[derive(Serialize)]
pub struct RevertJsonOutput {
    pub revert_type: String,
    pub revert_target: String,
    pub file_count: usize,
    pub stats: RevertStats,
    pub files: Vec<RevertFile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restored: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restored_files: Option<Vec<String>>,
}

#[derive(Serialize)]
pub struct RevertStats {
    pub additions: usize,
    pub deletions: usize,
    pub total: usize,
}

#[derive(Serialize)]
pub struct RevertFile {
    pub filename: String,
    pub status: String,
    pub additions: usize,
    pub deletions: usize,
    pub changes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
    pub event_id: i64,
    pub attribution: String,
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Restore a single file to the content represented by `target_hash`.
/// `target_hash = None` means "the file did not exist" — delete it.
///
/// Always records a pre-restore snapshot of the current file state first.
pub fn restore_file_to(store: &Store, rel_path: &str, target_hash: Option<&str>) -> Result<()> {
    let abs = store.paths.root.join(rel_path);
    let ts_ns = now_ns();

    // Capture current state so the restore is itself reversible.
    let current: Option<(String, i64, Vec<u8>)> = if abs.exists() && abs.is_file() {
        let bytes = fs::read(&abs).with_context(|| format!("reading current {}", abs.display()))?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let size = bytes.len() as i64;
        store.write_blob(&bytes)?;
        Some((hash, size, bytes))
    } else {
        None
    };

    // Record the pre-restore snapshot. Before and after are intentionally the
    // same hash — this event is a marker, not a content change.
    store.record_event(&NewEvent {
        ts_ns,
        path: rel_path.into(),
        before_hash: current.as_ref().map(|(h, _, _)| h.clone()),
        after_hash: current.as_ref().map(|(h, _, _)| h.clone()),
        size_before: current.as_ref().map(|(_, s, _)| *s),
        size_after: current.as_ref().map(|(_, s, _)| *s),
        attribution: "pre-restore".into(),
        confidence: "high".into(),
        session_id: None,
        pid: None,
        process_name: None,
        tool_name: None,
    })?;

    // Apply the target state.
    match target_hash {
        Some(h) => {
            let bytes = store
                .read_blob(h)
                .with_context(|| format!("reading blob {h}"))?;
            if let Some(parent) = abs.parent() {
                fs::create_dir_all(parent)?;
            }
            // Atomic write via temp + rename to match the store's invariant.
            let tmp = abs.with_extension(format!("agent-undo-restore.{}", &h[..8.min(h.len())]));
            fs::write(&tmp, &bytes)?;
            fs::rename(&tmp, &abs)?;

            let size = bytes.len() as i64;
            let after_ts = now_ns();
            store.record_event(&NewEvent {
                ts_ns: after_ts,
                path: rel_path.into(),
                before_hash: current.as_ref().map(|(h, _, _)| h.clone()),
                after_hash: Some(h.into()),
                size_before: current.as_ref().map(|(_, s, _)| *s),
                size_after: Some(size),
                attribution: "agent-undo-restore".into(),
                confidence: "high".into(),
                session_id: None,
                pid: None,
                process_name: None,
                tool_name: None,
            })?;
            store.upsert_file_state(rel_path, h, size, after_ts)?;
        }
        None => {
            // Target says "file didn't exist" — delete it if present.
            if abs.exists() {
                fs::remove_file(&abs).with_context(|| format!("removing {}", abs.display()))?;
            }
            let after_ts = now_ns();
            store.record_event(&NewEvent {
                ts_ns: after_ts,
                path: rel_path.into(),
                before_hash: current.as_ref().map(|(h, _, _)| h.clone()),
                after_hash: None,
                size_before: current.as_ref().map(|(_, s, _)| *s),
                size_after: None,
                attribution: "agent-undo-restore".into(),
                confidence: "high".into(),
                session_id: None,
                pid: None,
                process_name: None,
                tool_name: None,
            })?;
            store.delete_file_state(rel_path)?;
        }
    }

    Ok(())
}

/// Restore a file to its state BEFORE a given event happened. This is the
/// semantics of `agent-undo restore <event-id>` — "undo this event."
pub fn restore_to_event(store: &Store, event: &EventRow) -> Result<()> {
    restore_file_to(store, &event.path, event.before_hash.as_deref())
}

/// `restore --session S`: atomically roll back every file touched by a
/// session to its state BEFORE that session's first edit on that file.
///
/// For each file the session touched, we pick the *earliest* event in the
/// session (since sessions are ordered by ts_ns ascending) and restore to its
/// `before_hash`. That's the pre-session state of the file, which is exactly
/// what "undo this whole agent refactor" should mean.
pub fn restore_session(store: &Store, session_id: &str) -> Result<Vec<String>> {
    let events = store.events_for_session(session_id)?;
    if events.is_empty() {
        return Ok(vec![]);
    }
    // Walk the session in chronological order and keep the FIRST event per file.
    let mut earliest_per_file: std::collections::HashMap<String, EventRow> =
        std::collections::HashMap::new();
    for ev in events {
        earliest_per_file.entry(ev.path.clone()).or_insert(ev);
    }
    let mut restored = Vec::new();
    let mut paths: Vec<(String, EventRow)> = earliest_per_file.into_iter().collect();
    paths.sort_by(|a, b| a.0.cmp(&b.0));
    for (path, ev) in paths {
        restore_file_to(store, &path, ev.before_hash.as_deref())?;
        restored.push(path);
    }
    Ok(restored)
}

/// `restore --file F`: walk back to the state before the most recent
/// user-originated change on F.
pub fn restore_latest_change_to_file(store: &Store, rel_path: &str) -> Result<EventRow> {
    let ev = store
        .latest_user_event_for_file(rel_path)?
        .ok_or_else(|| anyhow::anyhow!("no undoable events for {rel_path}"))?;
    restore_to_event(store, &ev)?;
    Ok(ev)
}

/// `restore --pin <label>`: restore every file in the project to its state
/// at the moment the pin was created. Atomic across files; each one gets a
/// pre-restore safety snapshot.
pub fn restore_pin(store: &Store, label: &str) -> Result<Vec<String>> {
    let pin = store
        .find_pin(label)?
        .ok_or_else(|| anyhow::anyhow!("no pin labeled '{label}'"))?;
    let snapshot: BTreeMap<String, Option<String>> = store
        .file_state_at_event(pin.event_id)?
        .into_iter()
        .collect();
    let current_paths = store.current_tracked_paths()?;

    let mut targets = BTreeSet::new();
    for path in snapshot.keys() {
        let _ = targets.insert(path.clone());
    }
    for path in current_paths {
        let _ = targets.insert(path);
    }

    let mut restored = Vec::new();
    for path in targets {
        let target_hash = snapshot.get(&path).cloned().flatten();
        restore_file_to(store, &path, target_hash.as_deref())?;
        restored.push(path);
    }
    Ok(restored)
}

/// Plan and execute `oops`: walk back from the most recent user-originated
/// event, collecting everything within `window_ns`, and restore each affected
/// file to the oldest pre-state in that batch.
///
/// Returns the list of (rel_path, oldest_event_in_batch) actually applied.
pub fn oops(store: &Store, window_ns: i64) -> Result<Vec<(String, EventRow)>> {
    let batch = oops_plan(store, window_ns)?;
    for (path, ev) in &batch {
        restore_file_to(store, path, ev.before_hash.as_deref())?;
    }
    Ok(batch)
}

/// The read-only "what would oops do?" computation.
pub fn oops_plan(store: &Store, window_ns: i64) -> Result<Vec<(String, EventRow)>> {
    let events = store.recent_events(500)?;
    let undoable: Vec<&EventRow> = events
        .iter()
        .filter(|e| !is_restore_bookkeeping(&e.attribution))
        .collect();

    if undoable.is_empty() {
        return Ok(vec![]);
    }

    if let Some(session_id) = undoable[0].session_id.as_deref() {
        let session_events = store.events_for_session(session_id)?;
        if !session_events.is_empty() {
            let mut earliest_per_file: HashMap<String, EventRow> = HashMap::new();
            for ev in session_events {
                earliest_per_file.entry(ev.path.clone()).or_insert(ev);
            }
            let mut planned: Vec<(String, EventRow)> = earliest_per_file.into_iter().collect();
            planned.sort_by(|a, b| a.0.cmp(&b.0));
            return Ok(planned);
        }
    }

    let most_recent_ts = undoable[0].ts_ns;
    let cutoff = most_recent_ts - window_ns;

    // Events come newest-first. Walk forward (into the past) until we cross
    // the cutoff. For each file, remember the OLDEST event in the burst,
    // because its before_hash is "the state before the whole burst started."
    let mut earliest_per_file: HashMap<String, EventRow> = HashMap::new();
    for e in undoable.iter().take_while(|e| e.ts_ns >= cutoff) {
        // Newest-first iteration: later assignment wins → stores the oldest.
        earliest_per_file.insert(e.path.clone(), (*e).clone());
    }

    let mut planned: Vec<(String, EventRow)> = earliest_per_file.into_iter().collect();
    // Deterministic ordering for the confirm prompt.
    planned.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(planned)
}

fn is_restore_bookkeeping(attribution: &str) -> bool {
    matches!(
        attribution,
        "agent-undo-restore" | "pre-restore" | "initial-scan"
    )
}

/// Print a file's content from the store by event id + side.
pub fn show_event(store: &Store, event_id: i64, show_before: bool, show_after: bool) -> Result<()> {
    let bytes = show_event_bytes(store, event_id, show_before, show_after)?;
    // Write raw bytes to stdout; fall back to lossy if UTF-8 fails.
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    if lock.write_all(&bytes).is_err() {
        println!("{}", String::from_utf8_lossy(&bytes));
    }
    Ok(())
}

pub fn show_event_bytes(
    store: &Store,
    event_id: i64,
    show_before: bool,
    show_after: bool,
) -> Result<Vec<u8>> {
    let ev = store
        .get_event(event_id)?
        .ok_or_else(|| anyhow::anyhow!("no event #{event_id}"))?;

    // Default to after if neither flag given; to before if only --before.
    let want_before = show_before && !show_after;
    let (label, hash_opt) = if want_before {
        ("before", ev.before_hash.as_deref())
    } else {
        ("after", ev.after_hash.as_deref())
    };

    match hash_opt {
        Some(h) => store.read_blob(h),
        None => {
            bail!(
                "event #{event_id} has no {label} content ({} at this event)",
                if want_before {
                    "file did not exist"
                } else {
                    "file was deleted"
                }
            );
        }
    }
}

/// Print a unified diff for a single event.
pub fn diff_event(store: &Store, event_id: i64) -> Result<()> {
    let diff = diff_event_text(store, event_id)?;
    print!("{diff}");
    Ok(())
}

pub fn diff_event_text(store: &Store, event_id: i64) -> Result<String> {
    use similar::{ChangeTag, TextDiff};

    let ev = store
        .get_event(event_id)?
        .ok_or_else(|| anyhow::anyhow!("no event #{event_id}"))?;

    let before = read_blob_as_text(store, ev.before_hash.as_deref())?;
    let after = read_blob_as_text(store, ev.after_hash.as_deref())?;

    let diff = TextDiff::from_lines(&before, &after);
    let mut out = String::new();
    out.push_str(&format!("--- a/{} (event #{}, before)\n", ev.path, ev.id));
    out.push_str(&format!("+++ b/{} (event #{}, after)\n", ev.path, ev.id));
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => "-",
            ChangeTag::Insert => "+",
            ChangeTag::Equal => " ",
        };
        out.push_str(sign);
        out.push_str(&change.to_string());
    }
    Ok(out)
}

fn read_blob_as_text(store: &Store, hash: Option<&str>) -> Result<String> {
    match hash {
        Some(h) => {
            let bytes = store.read_blob(h)?;
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        }
        None => Ok(String::new()),
    }
}

// ═══════════════════════════════════════════════════════════════
// Revert plan functions
// ═══════════════════════════════════════════════════════════════

/// Plan a revert for a single event.
pub fn plan_event(store: &Store, event_id: i64) -> Result<Vec<PlanItem>> {
    let ev = store
        .get_event(event_id)?
        .ok_or_else(|| anyhow::anyhow!("no event #{event_id}"))?;
    let old_content = match &ev.before_hash {
        Some(h) => store.read_blob(h)?,
        None => Vec::new(),
    };
    let abs = store.paths.root.join(&ev.path);
    let new_content = if abs.exists() && abs.is_file() {
        fs::read(&abs).with_context(|| format!("reading {}", abs.display()))?
    } else {
        Vec::new()
    };
    let kind = match (&ev.before_hash, new_content.is_empty()) {
        (Some(_), false) => PlanKind::Modified,
        (None, false) => PlanKind::Created,
        (Some(_), true) => PlanKind::Deleted,
        (None, true) => PlanKind::Modified, // edge case
    };
    Ok(vec![PlanItem {
        path: ev.path,
        kind,
        old_content,
        new_content,
        target_hash: ev.before_hash,
        event_id: ev.id,
        attribution: ev.attribution,
    }])
}

/// Plan a revert for an entire session.
pub fn plan_session(store: &Store, session_id: &str) -> Result<Vec<PlanItem>> {
    let events = store.events_for_session(session_id)?;
    if events.is_empty() {
        return Ok(vec![]);
    }
    // Keep the FIRST event per file (its before_hash = pre-session state)
    let mut earliest_per_file: HashMap<String, EventRow> = HashMap::new();
    for ev in &events {
        earliest_per_file.entry(ev.path.clone()).or_insert_with(|| ev.clone());
    }
    let mut plan = Vec::new();
    let mut paths: Vec<String> = earliest_per_file.keys().cloned().collect();
    paths.sort();
    for path in paths {
        let ev = &earliest_per_file[&path];
        let old_content = match &ev.before_hash {
            Some(h) => store.read_blob(h)?,
            None => Vec::new(),
        };
        let abs = store.paths.root.join(&path);
        let new_content = if abs.exists() && abs.is_file() {
            fs::read(&abs).with_context(|| format!("reading {}", abs.display()))?
        } else {
            Vec::new()
        };
        let kind = match (&ev.before_hash, new_content.is_empty()) {
            (Some(_), false) => PlanKind::Modified,
            (None, false) => PlanKind::Created,
            (Some(_), true) => PlanKind::Deleted,
            (None, true) => PlanKind::Modified,
        };
        plan.push(PlanItem {
            path,
            kind,
            old_content,
            new_content,
            target_hash: ev.before_hash.clone(),
            event_id: ev.id,
            attribution: ev.attribution.clone(),
        });
    }
    Ok(plan)
}

/// Plan a revert to a pin point.
pub fn plan_pin(store: &Store, label: &str) -> Result<Vec<PlanItem>> {
    let pin = store
        .find_pin(label)?
        .ok_or_else(|| anyhow::anyhow!("no pin labeled '{label}'"))?;
    let snapshot: BTreeMap<String, Option<String>> = store
        .file_state_at_event(pin.event_id)?
        .into_iter()
        .collect();
    let current_paths = store.current_tracked_paths()?;
    let mut targets = BTreeSet::new();
    for path in snapshot.keys() {
        let _ = targets.insert(path.clone());
    }
    for path in current_paths {
        let _ = targets.insert(path);
    }
    let mut plan = Vec::new();
    for path in targets {
        let pin_hash = snapshot.get(&path).cloned().flatten();
        let old_content = match &pin_hash {
            Some(h) => store.read_blob(h)?,
            None => Vec::new(),
        };
        let abs = store.paths.root.join(&path);
        let new_content = if abs.exists() && abs.is_file() {
            fs::read(&abs).with_context(|| format!("reading {}", abs.display()))?
        } else {
            Vec::new()
        };
        let kind = match (&pin_hash, new_content.is_empty()) {
            (Some(_), false) => PlanKind::Modified,
            (None, false) => PlanKind::Created,
            (Some(_), true) => PlanKind::Deleted,
            (None, true) => PlanKind::Modified,
        };
        // Skip unchanged files
        if kind == PlanKind::Modified && old_content == new_content {
            continue;
        }
        // Skip files that daemon never tracked a deletion for.
        // file_state still has the row → daemon never called delete_file_state
        // → can't confirm deletion happened after pin → skip
        if kind == PlanKind::Deleted {
            if store.get_file_state(&path)?.is_some() {
                continue;
            }
        }
        plan.push(PlanItem {
            path,
            kind,
            old_content,
            new_content,
            target_hash: pin_hash,
            event_id: pin.event_id,
            attribution: String::new(),
        });
    }
    Ok(plan)
}

/// Format a plan for terminal output.
pub fn format_plan(plan: &[PlanItem], header: &str) -> String {
    use similar::{ChangeTag, TextDiff};

    let mut out = String::new();
    out.push_str(&format!("Would revert {} ({} file(s)):\n\n", header, plan.len()));

    for item in plan {
        let kind_str = match item.kind {
            PlanKind::Modified => "modified",
            PlanKind::Created => "added",
            PlanKind::Deleted => "removed",
        };
        let extra = match item.kind {
            PlanKind::Created => "  → will be deleted",
            PlanKind::Deleted => "  → will be restored",
            PlanKind::Modified => "",
        };
        out.push_str(&format!(
            "  {:10} {} (event #{}){}\n",
            kind_str, item.path, item.event_id, extra
        ));
    }
    out.push('\n');

    // Show diff for up to 20 files
    if plan.len() <= 20 {
        for item in plan {
            // Binary check
            let is_binary = item.old_content.contains(&0) || item.new_content.contains(&0);
            if is_binary {
                out.push_str(&format!(
                    "--- a/{} (current)\n+++ b/{} (target)\n(binary file, {} bytes → {} bytes)\n\n",
                    item.path,
                    item.path,
                    item.new_content.len(),
                    item.old_content.len()
                ));
                continue;
            }
            let old_text = String::from_utf8_lossy(&item.new_content);
            let new_text = String::from_utf8_lossy(&item.old_content);
            let diff = TextDiff::from_lines(&old_text, &new_text);
            out.push_str(&format!("--- a/{} (current)\n+++ b/{} (target)\n", item.path, item.path));
            for change in diff.iter_all_changes() {
                let sign = match change.tag() {
                    ChangeTag::Delete => "-",
                    ChangeTag::Insert => "+",
                    ChangeTag::Equal => " ",
                };
                out.push_str(sign);
                out.push_str(&change.to_string());
            }
            out.push('\n');
        }
    } else {
        out.push_str(&format!(
            "... (showing {} of {}, diff omitted for brevity)\n\n",
            20.min(plan.len()),
            plan.len()
        ));
    }
    out.push_str(&format!("Roll back these {} file(s)?", plan.len()));
    out
}

/// Convert a plan to JSON output.
pub fn plan_to_json(plan: &[PlanItem], revert_type: &str, revert_target: &str) -> RevertJsonOutput {
    use similar::{ChangeTag, TextDiff};

    let mut files = Vec::new();
    let mut total_additions = 0;
    let mut total_deletions = 0;

    for item in plan {
        let status = match item.kind {
            PlanKind::Modified => "modified",
            PlanKind::Created => "added",
            PlanKind::Deleted => "removed",
        };
        let is_binary = item.old_content.contains(&0) || item.new_content.contains(&0);
        let (patch, additions, deletions) = if is_binary {
            (None, 0, 0)
        } else {
            let old_text = String::from_utf8_lossy(&item.new_content);
            let new_text = String::from_utf8_lossy(&item.old_content);
            let diff = TextDiff::from_lines(&old_text, &new_text);
            let mut add = 0;
            let mut del = 0;
            let mut patch_str = String::new();
            for change in diff.iter_all_changes() {
                match change.tag() {
                    ChangeTag::Insert => add += 1,
                    ChangeTag::Delete => del += 1,
                    ChangeTag::Equal => {}
                }
                let sign = match change.tag() {
                    ChangeTag::Delete => "-",
                    ChangeTag::Insert => "+",
                    ChangeTag::Equal => " ",
                };
                patch_str.push_str(sign);
                patch_str.push_str(&change.to_string());
            }
            (Some(patch_str), add, del)
        };
        total_additions += additions;
        total_deletions += deletions;

        // Compute after_hash from current content
        let after_hash = if item.new_content.is_empty() {
            None
        } else {
            Some(blake3::hash(&item.new_content).to_hex().to_string())
        };

        files.push(RevertFile {
            filename: item.path.clone(),
            status: status.to_string(),
            additions,
            deletions,
            changes: additions + deletions,
            patch,
            before_hash: item.target_hash.clone(),
            after_hash,
            event_id: item.event_id,
            attribution: item.attribution.clone(),
        });
    }

    RevertJsonOutput {
        revert_type: revert_type.to_string(),
        revert_target: revert_target.to_string(),
        file_count: plan.len(),
        stats: RevertStats {
            additions: total_additions,
            deletions: total_deletions,
            total: total_additions + total_deletions,
        },
        files,
        restored: None,
        restored_files: None,
    }
}

/// Apply a revert plan (reuses existing `restore_file_to`).
pub fn apply_plan(store: &Store, plan: &[PlanItem]) -> Result<Vec<String>> {
    let mut restored = Vec::new();
    for item in plan {
        restore_file_to(store, &item.path, item.target_hash.as_deref())?;
        restored.push(item.path.clone());
    }
    Ok(restored)
}

#[cfg(test)]
mod tests {
    use super::oops_plan;
    use crate::paths::ProjectPaths;
    use crate::store::{NewEvent, Store};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_tmp_dir(label: &str) -> PathBuf {
        let ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("agent-undo-restore-unit-{label}-{pid}-{ns}"));
        fs::create_dir_all(&dir).expect("create tmp dir");
        dir
    }

    #[test]
    fn oops_plan_prefers_full_explicit_session_over_time_window() {
        let dir = unique_tmp_dir("oops_session");
        let store = Store::init(ProjectPaths::for_root(dir.clone())).expect("init store");

        store
            .record_event(&NewEvent {
                ts_ns: 100,
                path: "a.txt".into(),
                before_hash: None,
                after_hash: Some("hash-a".into()),
                size_before: None,
                size_after: Some(1),
                attribution: "test-agent".into(),
                confidence: "high".into(),
                session_id: Some("session-1".into()),
                pid: None,
                process_name: None,
                tool_name: None,
            })
            .expect("record first event");
        store
            .record_event(&NewEvent {
                ts_ns: 200,
                path: "b.txt".into(),
                before_hash: None,
                after_hash: Some("hash-b".into()),
                size_before: None,
                size_after: Some(1),
                attribution: "test-agent".into(),
                confidence: "high".into(),
                session_id: Some("session-1".into()),
                pid: None,
                process_name: None,
                tool_name: None,
            })
            .expect("record second event");

        let plan = oops_plan(&store, 1).expect("build oops plan");
        let paths: Vec<String> = plan.into_iter().map(|(path, _)| path).collect();
        assert_eq!(paths, vec!["a.txt".to_string(), "b.txt".to_string()]);

        fs::remove_dir_all(&dir).ok();
    }
}
