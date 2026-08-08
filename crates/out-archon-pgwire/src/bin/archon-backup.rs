//! `archon-backup` — offline backup / restore for an Archon database file.
//!
//! Usage:
//!   archon-backup backup  <db-path> <dest-dir>   # snapshot data + .wal sidecar
//!   archon-backup restore <snapshot-dir> <db-path> # restore into place
//!   archon-backup list   <dest-dir>               # list snapshot directories

use std::process::exit;

use out_archon_pgwire::backup;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "usage:\n  archon-backup backup <db-path> <dest-dir>\n  \
             archon-backup restore <snapshot-dir> <db-path>\n  \
             archon-backup list <dest-dir>"
        );
        exit(2);
    }

    let cmd = args[1].as_str();
    let result = match cmd {
        "backup" if args.len() >= 4 => {
            let snap = backup::snapshot(
                std::path::Path::new(&args[2]),
                std::path::Path::new(&args[3]),
            );
            snap.map(|p| format!("snapshot written to {}", p.display()))
        }
        "restore" if args.len() >= 4 => backup::restore(
            std::path::Path::new(&args[2]),
            std::path::Path::new(&args[3]),
        )
        .map(|_| "restored".to_string()),
        "list" if args.len() >= 3 => {
            backup::list_snapshots(std::path::Path::new(&args[2])).map(|snaps| {
                if snaps.is_empty() {
                    "no snapshots found".to_string()
                } else {
                    snaps
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            })
        }
        _ => {
            eprintln!("unknown command or wrong argument count: {}", cmd);
            exit(2);
        }
    };

    match result {
        Ok(msg) => {
            println!("{}", msg);
        }
        Err(e) => {
            eprintln!("error: {}", e);
            exit(1);
        }
    }
}
