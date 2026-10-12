//! Per-repo paging cursor state for `kin locate --next`.
//!
//! `kin locate` returns page 0 plus an opaque next-page cursor; a follow-up `kin
//! locate --next` resumes from it. The cursor token is carried between the two
//! invocations through a small local state file (`.kin/locate-cursor`). This is
//! CLI paging state, not a semantic answer authority: the token is opaque and the
//! ranking it indexes lives in the daemon's page cache, so this module never reads
//! repo content or answers a query from the filesystem.

use anyhow::Result;

/// Path of the per-repo locate cursor file (`.kin/locate-cursor`), used to carry
/// the next-page cursor between a `kin locate` and a follow-up `kin locate
/// --next`.
pub fn locate_cursor_path(layout: &kin_core::KinLayout) -> std::path::PathBuf {
    layout.root().join("locate-cursor")
}

/// Persist (or clear) the next-page cursor for `--next`. Best-effort: any IO
/// error is ignored so paging never breaks the result.
pub fn persist_locate_cursor(next_cursor: Option<&str>) {
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    let Some(layout) = kin_core::KinLayout::discover(&cwd) else {
        return;
    };
    let Ok(store) = cursor_store(&layout) else {
        return;
    };
    match next_cursor {
        Some(cursor) => {
            let _ = store.write_scratch_record(LOCATE_CURSOR_RECORD, cursor.as_bytes());
        }
        // No further pages: clear any stale cursor so a later bare `--next`
        // fails loud instead of paging a dead ranking.
        None => {
            let _ = store.remove_record(LOCATE_CURSOR_RECORD);
        }
    }
}

/// The cursor's record name in the store, beside [`locate_cursor_path`].
const LOCATE_CURSOR_RECORD: &str = "locate-cursor";

/// The longest cursor read back. A cursor is a short continuation token.
const MAX_LOCATE_CURSOR_BYTES: u64 = 64 * 1024;

/// The store's own records. The cursor is a record of this store, never
/// repository content, so the capability that holds it reaches the store and
/// nothing outside it.
fn cursor_store(layout: &kin_core::KinLayout) -> std::io::Result<kin_io::StoreDir> {
    kin_io::StoreDir::open(layout.root(), &kin_io::Grant::ambient())
}

/// Read the persisted next-page cursor for `kin locate --next`. Errors loud when
/// absent: a `--next` with no prior page is a user error, not a silent empty.
pub fn read_persisted_locate_cursor() -> Result<String> {
    let cwd = std::env::current_dir()?;
    let layout = crate::commands::require_repository_layout_at(&cwd)?;
    let no_page = || {
        anyhow::anyhow!(
            "no locate page to advance: run `kin locate <query>` first, then `kin locate --next`"
        )
    };
    let cursor = cursor_store(&layout)
        .and_then(|store| store.read_record(LOCATE_CURSOR_RECORD, MAX_LOCATE_CURSOR_BYTES))
        .ok()
        .flatten()
        .ok_or_else(no_page)?;
    let cursor = String::from_utf8(cursor).map_err(|_| no_page())?;
    let cursor = cursor.trim().to_string();
    if cursor.is_empty() {
        anyhow::bail!("no further locate pages (the previous page was the last)");
    }
    Ok(cursor)
}
