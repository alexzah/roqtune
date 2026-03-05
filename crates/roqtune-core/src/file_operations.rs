//! Async batch file copy/move operations.
//!
//! The [`BatchFileOperationManager`] subscribes to the event bus, handles
//! [`LibraryMessage::StartBatchFileOperation`] by spawning an OS thread for
//! the actual I/O, and reports progress and completion via the bus.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use log::{info, warn};
use tokio::sync::broadcast::{Receiver, Sender};

use crate::protocol::{BatchFileMode, BatchFileResult, BatchFileTarget, LibraryMessage, Message};

type CancelToken = Arc<AtomicBool>;

struct CancelRegistration {
    request_id: u64,
    tokens: Arc<Mutex<HashMap<u64, CancelToken>>>,
}

impl Drop for CancelRegistration {
    fn drop(&mut self) {
        if let Ok(mut map) = self.tokens.lock() {
            map.remove(&self.request_id);
        }
    }
}

/// Checks whether a destination path is syntactically valid (non-empty, valid
/// UTF-8, no null bytes). Does not perform I/O.
pub fn is_valid_dest_path(path: &Path) -> bool {
    match path.to_str() {
        None => false,
        Some("") => false,
        Some(s) => !s.contains('\0'),
    }
}

/// Background service that runs batch file copy/move operations.
pub struct BatchFileOperationManager {
    bus_consumer: Receiver<Message>,
    bus_producer: Sender<Message>,
    cancel_tokens: Arc<Mutex<HashMap<u64, CancelToken>>>,
}

impl BatchFileOperationManager {
    /// Creates a new manager bound to the event bus.
    pub fn new(bus_consumer: Receiver<Message>, bus_producer: Sender<Message>) -> Self {
        Self {
            bus_consumer,
            bus_producer,
            cancel_tokens: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Runs the event loop until the bus closes.
    pub fn run(&mut self) {
        loop {
            match self.bus_consumer.blocking_recv() {
                Ok(Message::Library(msg)) => self.handle_library_message(msg),
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }

    fn handle_library_message(&mut self, msg: LibraryMessage) {
        match msg {
            LibraryMessage::StartBatchFileOperation {
                request_id,
                mode,
                targets,
                move_folder_contents,
            } => {
                self.spawn_batch_operation(request_id, mode, targets, move_folder_contents);
            }
            LibraryMessage::BatchFileOperationAbort { request_id } => {
                self.request_abort(request_id);
            }
            _ => {}
        }
    }

    fn request_abort(&self, request_id: u64) {
        if let Ok(map) = self.cancel_tokens.lock() {
            if let Some(token) = map.get(&request_id) {
                token.store(true, Ordering::Relaxed);
            }
        }
    }

    fn spawn_batch_operation(
        &self,
        request_id: u64,
        mode: BatchFileMode,
        targets: Vec<BatchFileTarget>,
        move_folder_contents: bool,
    ) {
        let bus_producer = self.bus_producer.clone();
        let cancel_token = Arc::new(AtomicBool::new(false));

        if let Ok(mut map) = self.cancel_tokens.lock() {
            map.insert(request_id, cancel_token.clone());
        }

        let cancel_tokens = self.cancel_tokens.clone();
        let spawn_result = std::thread::Builder::new()
            .name(format!("batch-file-op-{request_id}"))
            .spawn(move || {
                let _reg = CancelRegistration {
                    request_id,
                    tokens: cancel_tokens,
                };
                let result = panic::catch_unwind(AssertUnwindSafe(|| {
                    run_batch_operation(
                        &bus_producer,
                        request_id,
                        mode,
                        targets,
                        move_folder_contents,
                        cancel_token,
                    );
                }));
                if let Err(payload) = result {
                    let error = payload
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".to_string());
                    warn!("Batch file operation worker panicked: {error}");
                    let _ = bus_producer.send(Message::Library(
                        LibraryMessage::BatchFileOperationCompleted {
                            request_id,
                            results: vec![],
                        },
                    ));
                }
            });

        if let Err(error) = spawn_result {
            if let Ok(mut map) = self.cancel_tokens.lock() {
                map.remove(&request_id);
            }
            warn!("Failed to spawn batch file operation thread: {error}");
            let _ = self.bus_producer.send(Message::Library(
                LibraryMessage::BatchFileOperationCompleted {
                    request_id,
                    results: vec![],
                },
            ));
        }
    }
}

fn run_batch_operation(
    bus_producer: &Sender<Message>,
    request_id: u64,
    mode: BatchFileMode,
    targets: Vec<BatchFileTarget>,
    move_folder_contents: bool,
    cancel_token: CancelToken,
) {
    let total = targets.len();
    let mut results: Vec<BatchFileResult> = Vec::with_capacity(total);

    for (idx, target) in targets.iter().enumerate() {
        if cancel_token.load(Ordering::Relaxed) {
            break;
        }

        let current = target
            .source_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        let _ = bus_producer.send(Message::Library(
            LibraryMessage::BatchFileOperationProgress {
                request_id,
                processed: idx,
                total,
                current: current.clone(),
            },
        ));

        let outcome = execute_single_operation(mode, &target.source_path, &target.dest_path);
        let (success, error) = match outcome {
            Ok(()) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        };

        if success {
            info!(
                "Batch file op: {} {:?} -> {:?}",
                if mode == BatchFileMode::Move {
                    "moved"
                } else {
                    "copied"
                },
                target.source_path,
                target.dest_path
            );
        } else {
            warn!(
                "Batch file op failed for {:?}: {:?}",
                target.source_path,
                error.as_deref()
            );
        }

        results.push(BatchFileResult {
            source_path: target.source_path.clone(),
            dest_path: target.dest_path.clone(),
            success,
            error,
        });
    }

    // Move/copy companion folder files (e.g. cover art) if requested.
    if move_folder_contents && !cancel_token.load(Ordering::Relaxed) {
        let extra = run_folder_contents_operation(mode, &targets, &cancel_token);
        results.extend(extra);
    }

    // Send final progress at 100% before completing.
    let completed = results.len();
    let _ = bus_producer.send(Message::Library(
        LibraryMessage::BatchFileOperationProgress {
            request_id,
            processed: completed,
            total: completed,
            current: String::new(),
        },
    ));

    let _ = bus_producer.send(Message::Library(
        LibraryMessage::BatchFileOperationCompleted {
            request_id,
            results,
        },
    ));
}

/// Moves or copies non-track files (e.g. `cover.jpg`) from each source directory
/// alongside the tracks. Each directory's companion files follow the destination
/// folder of the lexicographically-first track filename in that directory.
fn run_folder_contents_operation(
    mode: BatchFileMode,
    targets: &[BatchFileTarget],
    cancel_token: &CancelToken,
) -> Vec<BatchFileResult> {
    // source_dir -> BTreeMap<source_filename, dest_dir>
    // BTreeMap keeps filenames sorted so .values().next() is the lex-first entry.
    let mut dir_map: HashMap<PathBuf, BTreeMap<String, PathBuf>> = HashMap::new();

    for target in targets {
        let Some(source_dir) = target.source_path.parent() else {
            continue;
        };
        let Some(dest_dir) = target.dest_path.parent() else {
            continue;
        };
        let source_name = target
            .source_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        dir_map
            .entry(source_dir.to_path_buf())
            .or_default()
            .insert(source_name, dest_dir.to_path_buf());
    }

    let track_sources: HashSet<&PathBuf> = targets.iter().map(|t| &t.source_path).collect();
    let mut results = Vec::new();

    for (source_dir, track_map) in &dir_map {
        if cancel_token.load(Ordering::Relaxed) {
            break;
        }

        // Lex-first track's destination folder.
        let Some(dest_dir) = track_map.values().next() else {
            continue;
        };

        let entries = match std::fs::read_dir(source_dir) {
            Ok(e) => e,
            Err(err) => {
                warn!("Batch folder contents: can't read {:?}: {err}", source_dir);
                continue;
            }
        };

        for entry in entries.flatten() {
            if cancel_token.load(Ordering::Relaxed) {
                break;
            }

            let source = entry.path();
            if !source.is_file() || track_sources.contains(&source) {
                continue;
            }

            let Some(filename) = source.file_name() else {
                continue;
            };
            let dest = dest_dir.join(filename);

            let outcome = execute_single_operation(mode, &source, &dest);
            let (success, error) = match outcome {
                Ok(()) => (true, None),
                Err(e) => (false, Some(e.to_string())),
            };

            if success {
                info!("Batch folder contents: {:?} -> {:?}", source, dest);
            } else {
                warn!(
                    "Batch folder contents failed for {:?}: {:?}",
                    source,
                    error.as_deref()
                );
            }

            results.push(BatchFileResult {
                source_path: source,
                dest_path: dest,
                success,
                error,
            });
        }
    }

    results
}

fn execute_single_operation(
    mode: BatchFileMode,
    source: &Path,
    dest: &Path,
) -> std::io::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match mode {
        BatchFileMode::Copy => {
            std::fs::copy(source, dest)?;
        }
        BatchFileMode::Move => {
            // Try rename first (same filesystem, fast). Fall back to copy+delete.
            if std::fs::rename(source, dest).is_err() {
                std::fs::copy(source, dest)?;
                std::fs::remove_file(source)?;
            }
        }
    }
    Ok(())
}
