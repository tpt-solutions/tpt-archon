//! Offline backup / restore for an Archon database file.
//!
//! An Archon database on disk is a pair of files: the data file at `db_path`
//! and its write-ahead-log sidecar at `db_path.wal` (see Phase 12.1). A
//! consistent backup copies both atomsically into a timestamped snapshot
//! directory; `restore` copies them back.
//!
//! This is the Phase 12.6 "backup / restore tooling" v1: a checkpoint-level
//! file copy. Point-in-time recovery (WAL shipping / archive replay) and
//! streaming replication remain explicit follow-ups and are documented as such
//! in `TODO.md`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Returns the `.wal` sidecar path for a data `db_path`.
pub fn wal_path(db_path: &Path) -> PathBuf {
    let mut p = db_path.as_os_str().to_os_string();
    p.push(".wal");
    PathBuf::from(p)
}

/// Copies `db_path` and its `.wal` sidecar (if present) into a new snapshot
/// directory under `dest_dir`. The snapshot directory name is
/// `archon-backup-<unix-secs>`. Returns the snapshot directory path.
///
/// Either the data file or its sidecar may be absent (a freshly created,
/// uncommitted database may have neither yet), but `db_path` itself must name
/// the logical database.
pub fn snapshot(db_path: &Path, dest_dir: &Path) -> io::Result<PathBuf> {
    if !dest_dir.exists() {
        fs::create_dir_all(dest_dir)?;
    }
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let snap = dest_dir.join(format!("archon-backup-{}", secs));
    fs::create_dir_all(&snap)?;

    copy_if_exists(db_path, &snap.join(db_path.file_name().unwrap_or_default()))?;
    let wp = wal_path(db_path);
    copy_if_exists(&wp, &snap.join(wp.file_name().unwrap_or_default()))?;

    Ok(snap)
}

/// Restores a database from a `snapshot_dir` produced by [`snapshot`], copying
/// the data file and its `.wal` sidecar back to `db_path`. Any existing
/// destination files are overwritten.
pub fn restore(snapshot_dir: &Path, db_path: &Path) -> io::Result<()> {
    let src_data = snapshot_dir.join(db_path.file_name().unwrap_or_default());
    copy_overwrite(&src_data, db_path)?;

    let wp = wal_path(db_path);
    let src_wal = snapshot_dir.join(wp.file_name().unwrap_or_default());
    // The sidecar is optional: a backup taken before the first commit has none.
    if src_wal.exists() {
        copy_overwrite(&src_wal, &wp)?;
    } else if wp.exists() {
        fs::remove_file(&wp)?;
    }
    Ok(())
}

/// Lists snapshot directories under `dest_dir` in creation order (oldest
/// first), suitable for choosing a restore target or for retention policies.
pub fn list_snapshots(dest_dir: &Path) -> io::Result<Vec<PathBuf>> {
    if !dest_dir.exists() {
        return Ok(Vec::new());
    }
    let mut out: Vec<PathBuf> = fs::read_dir(dest_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("archon-backup-"))
                .unwrap_or(false)
        })
        .collect();
    out.sort();
    Ok(out)
}

fn copy_if_exists(src: &Path, dst: &Path) -> io::Result<()> {
    if src.exists() {
        copy_overwrite(src, dst)
    } else {
        Ok(())
    }
}

fn copy_overwrite(src: &Path, dst: &Path) -> io::Result<()> {
    if let Some(parent) = dst.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::copy(src, dst).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let mut p = std::env::temp_dir();
        let unique = format!(
            "archon-backup-test-{}-{}",
            std::process::id(),
            // a per-call counter isn't needed; pid + nanos is plenty unique
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        p.push(unique);
        p
    }

    #[test]
    fn snapshot_and_restore_round_trip() {
        let base = tmp();
        let db = base.join("mydb.archon");
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        fs::write(&db, b"data-bytes").unwrap();
        fs::write(wal_path(&db), b"wal-bytes").unwrap();

        let backups = base.join("backups");
        let snap = snapshot(&db, &backups).unwrap();
        assert!(snap.exists());

        // Mutate the live database, then restore from the snapshot.
        fs::write(&db, b"corrupted").unwrap();
        restore(&snap, &db).unwrap();
        assert_eq!(fs::read(&db).unwrap(), b"data-bytes");
        assert_eq!(fs::read(wal_path(&db)).unwrap(), b"wal-bytes");

        let snaps = list_snapshots(&backups).unwrap();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0], snap);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn snapshot_without_wal_still_restores_data() {
        let base = tmp();
        let db = base.join("nolog.archon");
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        fs::write(&db, b"only-data").unwrap();

        let backups = base.join("backups2");
        let snap = snapshot(&db, &backups).unwrap();
        fs::remove_file(&db).unwrap();
        restore(&snap, &db).unwrap();
        assert_eq!(fs::read(&db).unwrap(), b"only-data");
        assert!(!wal_path(&db).exists());

        let _ = fs::remove_dir_all(&base);
    }
}
