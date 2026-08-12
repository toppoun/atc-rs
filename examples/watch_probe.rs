use notify::{EventKind, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

fn main() -> notify::Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);

    let (tx, rx) = mpsc::channel();

    let mut watcher = notify::recommended_watcher(move |result| {
        let _ = tx.send(result);
    })?;

    watcher.watch(&path, RecursiveMode::NonRecursive)?;

    println!("Watching: {}", path.display());

    loop {
        let first = match rx.recv() {
            Ok(result) => result,
            Err(_) => break,
        };

        let mut pending = HashSet::new();

        if let Ok(event) = first {
            collect_changed_paths(&event.kind, event.paths, &mut pending);
        }

        loop {
            match rx.recv_timeout(Duration::from_millis(150)) {
                Ok(Ok(event)) => {
                    collect_changed_paths(&event.kind, event.paths, &mut pending);
                }

                Ok(Err(error)) => {
                    eprintln!("watch error: {error}");
                }

                Err(mpsc::RecvTimeoutError::Timeout) => {
                    break;
                }

                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Ok(());
                }
            }
        }

        for path in pending {
            println!("saved: {}", path.display());
        }
    }

    Ok(())
}

fn collect_changed_paths(kind: &EventKind, paths: Vec<PathBuf>, pending: &mut HashSet<PathBuf>) {
    if !matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    ) {
        return;
    }

    for path in paths {
        pending.insert(path);
    }
}
