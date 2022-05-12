//! High-throughput parallel directory scanner.
//! Uses a lock-free MPMC queue (crossbeam-channel) + thread-local accumulators
//! so workers never fight over mutexes while chewing through NTFS.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender, unbounded};
use rustc_hash::FxHashMap;

use crate::types::{
    ExtStat, FileCategory, FileEntry, FolderEntry, ScanMode, ScanReport, TopK, WorkerLocal,
    classify_ext,
};
use crate::win_fs::{
    append_wildcard, attr_flags, enumerate_dir, filetime_to_unix, is_dir, is_reparse_point,
    logical_cpu_count, query_allocated_size, query_volume_info, to_extended_utf16,
};

pub struct LiveProgress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub bytes: AtomicU64,
    pub errors: AtomicU64,
    pub cancel: AtomicBool,
    pub done: AtomicBool,
}

impl LiveProgress {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            files: AtomicU64::new(0),
            dirs: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            cancel: AtomicBool::new(false),
            done: AtomicBool::new(false),
        })
    }
}

struct DirRecord {
    path: String,
    direct_bytes: u64,
    direct_allocated: u64,
    total_bytes: u64,
    total_allocated: u64,
    file_count: u64,
}

struct WorkerOutput {
    local: WorkerLocal,
    dirs: Vec<DirRecord>,
}

/// Check hardware backend. Truth is, Windows NTFS metadata syscalls (FindFirstFileExW)
/// have to go through the OS kernel on CPU, so if no GPU compute offload is needed/present
/// we spin up a fat CPU worker pool tuned for SSD/NVMe queue depth.
pub fn detect_compute_backend(prefer_gpu: bool, workers: usize) -> String {
    if prefer_gpu {
        // Honest fallback: filesystem syscalls cannot run inside a GPU shader anyway,
        // so we route straight to parallel CPU workers with zero drama.
        format!("CPU-IO Pool ({} workers, GPU not used for NTFS syscalls)", workers)
    } else {
        format!("CPU-IO Pool ({} workers)", workers)
    }
}

pub fn default_worker_count() -> usize {
    let cpus = logical_cpu_count();
    // 2x logical cores keeps NVMe/SSD command queues saturated without thrashing.
    (cpus * 2).clamp(4, 32)
}

pub fn run_scan(
    root: &str,
    mode: ScanMode,
    workers: Option<usize>,
    top_k: usize,
    prefer_gpu: bool,
    progress: Arc<LiveProgress>,
) -> ScanReport {
    let start = Instant::now();
    let num_workers = workers.unwrap_or_else(default_worker_count).max(1);
    let compute_backend = detect_compute_backend(prefer_gpu, num_workers);

    let normalized_root = normalize_root(root);
    let volume = query_volume_info(&normalized_root);

    let (tx, rx): (Sender<Option<String>>, Receiver<Option<String>>) = unbounded();
    let active_jobs = Arc::new(AtomicUsize::new(1));
    tx.send(Some(normalized_root.clone())).ok();

    let mut handles = Vec::with_capacity(num_workers);
    for _ in 0..num_workers {
        let rx_c = rx.clone();
        let tx_c = tx.clone();
        let active_c = Arc::clone(&active_jobs);
        let prog_c = Arc::clone(&progress);
        handles.push(thread::spawn(move || {
            worker_loop(rx_c, tx_c, active_c, prog_c, mode, top_k, num_workers)
        }));
    }
    drop(tx);
    drop(rx);

    let mut merged = WorkerLocal::new(top_k);
    let mut all_dirs: Vec<DirRecord> = Vec::with_capacity(4096);

    for handle in handles {
        if let Ok(out) = handle.join() {
            merged.total_files += out.local.total_files;
            merged.total_dirs += out.local.total_dirs;
            merged.total_logical_bytes += out.local.total_logical_bytes;
            merged.total_allocated_bytes += out.local.total_allocated_bytes;
            merged.access_denied_count += out.local.access_denied_count;
            merged.reparse_skipped_count += out.local.reparse_skipped_count;

            for i in 0..FileCategory::COUNT {
                merged.categories[i].count += out.local.categories[i].count;
                merged.categories[i].logical_bytes += out.local.categories[i].logical_bytes;
                merged.categories[i].allocated_bytes += out.local.categories[i].allocated_bytes;
            }

            for (ext, agg) in out.local.extensions {
                let entry = merged.extensions.entry(ext).or_default();
                entry.count += agg.count;
                entry.logical_bytes += agg.logical_bytes;
                entry.allocated_bytes += agg.allocated_bytes;
            }

            for f in out.local.top_files.into_sorted_desc() {
                merged.top_files.push(f);
            }

            merged.attrs.hidden_count += out.local.attrs.hidden_count;
            merged.attrs.hidden_bytes += out.local.attrs.hidden_bytes;
            merged.attrs.system_count += out.local.attrs.system_count;
            merged.attrs.system_bytes += out.local.attrs.system_bytes;
            merged.attrs.readonly_count += out.local.attrs.readonly_count;
            merged.attrs.compressed_count += out.local.attrs.compressed_count;
            merged.attrs.sparse_count += out.local.attrs.sparse_count;
            merged.attrs.offline_cloud_count += out.local.attrs.offline_cloud_count;

            all_dirs.extend(out.dirs);
        }
    }

    // Bottom-up folder aggregation: sort deepest paths first so every child adds
    // its total into its parent in a single linear pass.
    let top_folders = aggregate_folders(all_dirs, top_k);

    let mut top_extensions: Vec<ExtStat> = merged
        .extensions
        .into_iter()
        .map(|(ext, agg)| ExtStat {
            ext,
            count: agg.count,
            bytes: agg.logical_bytes,
        })
        .collect();
    top_extensions.sort_unstable_by(|a, b| b.bytes.cmp(&a.bytes));
    top_extensions.truncate(top_k);

    progress.done.store(true, Ordering::Release);

    ScanReport {
        root: normalized_root,
        mode,
        workers_used: num_workers,
        compute_backend,
        elapsed_secs: start.elapsed().as_secs_f64(),
        total_files: merged.total_files,
        total_dirs: merged.total_dirs,
        total_logical_bytes: merged.total_logical_bytes,
        total_allocated_bytes: merged.total_allocated_bytes,
        access_denied_count: merged.access_denied_count,
        reparse_skipped_count: merged.reparse_skipped_count,
        categories: merged.categories,
        top_files: merged.top_files.into_sorted_desc(),
        top_folders,
        top_extensions,
        attrs: merged.attrs,
        volume,
    }
}

fn worker_loop(
    rx: Receiver<Option<String>>,
    tx: Sender<Option<String>>,
    active_jobs: Arc<AtomicUsize>,
    progress: Arc<LiveProgress>,
    mode: ScanMode,
    top_k: usize,
    num_workers: usize,
) -> WorkerOutput {
    let mut local = WorkerLocal::new(top_k);
    let mut dirs = Vec::with_capacity(512);

    // Reusable UTF-16 scratch buffers so we don't malloc on every single file.
    let mut dir_utf16 = Vec::with_capacity(512);
    let mut search_utf16 = Vec::with_capacity(512);
    let mut file_utf16 = Vec::with_capacity(512);

    let (attr_ro, attr_hid, attr_sys, attr_comp, attr_sparse, attr_offline) = attr_flags();

    while let Ok(Some(dir_path)) = rx.recv() {
        if progress.cancel.load(Ordering::Relaxed) {
            if active_jobs.fetch_sub(1, Ordering::AcqRel) == 1 {
                for _ in 0..num_workers {
                    let _ = tx.send(None);
                }
            }
            continue;
        }

        local.total_dirs += 1;
        progress.dirs.fetch_add(1, Ordering::Relaxed);

        to_extended_utf16(&dir_path, &mut dir_utf16);
        // Drop trailing nul so we can append `\*` or `\filename`
        if dir_utf16.last() == Some(&0) {
            dir_utf16.pop();
        }
        append_wildcard(&dir_utf16, &mut search_utf16);

        let mut direct_bytes: u64 = 0;
        let mut direct_allocated: u64 = 0;
        let mut dir_file_count: u64 = 0;

        // Batch local counters and flush to atomics once per directory to avoid cache-line ping-pong.
        let mut batch_files: u64 = 0;
        let mut batch_bytes: u64 = 0;

        let res = enumerate_dir(&search_utf16, |name_utf16, data| {
            let attrs = data.dwFileAttributes;

            if is_dir(attrs) {
                if is_reparse_point(attrs) {
                    // Skip NTFS junctions/symlinks so we don't loop forever in AppData.
                    local.reparse_skipped_count += 1;
                    return;
                }
                let child_name = String::from_utf16_lossy(name_utf16);
                let child_path = join_win_path(&dir_path, &child_name);
                active_jobs.fetch_add(1, Ordering::AcqRel);
                if tx.send(Some(child_path)).is_err() {
                    active_jobs.fetch_sub(1, Ordering::AcqRel);
                }
                return;
            }

            let logical = ((data.nFileSizeHigh as u64) << 32) | (data.nFileSizeLow as u64);
            let allocated = if mode == ScanMode::Detailed {
                file_utf16.clear();
                file_utf16.extend_from_slice(&dir_utf16);
                if file_utf16.last() != Some(&(b'\\' as u16)) {
                    file_utf16.push(b'\\' as u16);
                }
                file_utf16.extend_from_slice(name_utf16);
                file_utf16.push(0);
                query_allocated_size(&file_utf16, logical)
            } else {
                logical
            };

            let name_str = String::from_utf16_lossy(name_utf16);
            let ext = extract_ext(&name_str);
            let cat = classify_ext(ext);
            let cat_agg = &mut local.categories[cat as usize];
            cat_agg.count += 1;
            cat_agg.logical_bytes += logical;
            cat_agg.allocated_bytes += allocated;

            let ext_key = if ext.is_empty() {
                "(none)".to_string()
            } else {
                ext.to_ascii_lowercase()
            };
            let ext_agg = local.extensions.entry(ext_key).or_default();
            ext_agg.count += 1;
            ext_agg.logical_bytes += logical;
            ext_agg.allocated_bytes += allocated;

            if mode == ScanMode::Detailed {
                if attrs & attr_hid != 0 {
                    local.attrs.hidden_count += 1;
                    local.attrs.hidden_bytes += logical;
                }
                if attrs & attr_sys != 0 {
                    local.attrs.system_count += 1;
                    local.attrs.system_bytes += logical;
                }
                if attrs & attr_ro != 0 {
                    local.attrs.readonly_count += 1;
                }
                if attrs & attr_comp != 0 {
                    local.attrs.compressed_count += 1;
                }
                if attrs & attr_sparse != 0 {
                    local.attrs.sparse_count += 1;
                }
                if attrs & attr_offline != 0 {
                    local.attrs.offline_cloud_count += 1;
                }
            }

            let full_path = join_win_path(&dir_path, &name_str);
            local.top_files.push(FileEntry {
                logical_bytes: logical,
                allocated_bytes: allocated,
                modified_unix: if mode == ScanMode::Detailed {
                    filetime_to_unix(data.ftLastWriteTime)
                } else {
                    0
                },
                attrs,
                path: full_path,
            });

            direct_bytes += logical;
            direct_allocated += allocated;
            dir_file_count += 1;
            batch_files += 1;
            batch_bytes += logical;
        });

        if res.access_denied {
            local.access_denied_count += 1;
            progress.errors.fetch_add(1, Ordering::Relaxed);
        }

        local.total_files += batch_files;
        local.total_logical_bytes += batch_bytes;
        local.total_allocated_bytes += direct_allocated;
        if batch_files > 0 {
            progress.files.fetch_add(batch_files, Ordering::Relaxed);
            progress.bytes.fetch_add(batch_bytes, Ordering::Relaxed);
        }

        dirs.push(DirRecord {
            path: dir_path,
            direct_bytes,
            direct_allocated,
            total_bytes: direct_bytes,
            total_allocated: direct_allocated,
            file_count: dir_file_count,
        });

        if active_jobs.fetch_sub(1, Ordering::AcqRel) == 1 {
            // Last active directory finished! Wake up all workers with poison pills.
            for _ in 0..num_workers {
                let _ = tx.send(None);
            }
        }
    }

    WorkerOutput { local, dirs }
}

fn aggregate_folders(mut dirs: Vec<DirRecord>, top_k: usize) -> Vec<FolderEntry> {
    if dirs.is_empty() {
        return Vec::new();
    }

    // Sort deepest paths first (by number of backslashes descending)
    dirs.sort_unstable_by(|a, b| {
        let da = a.path.bytes().filter(|&c| c == b'\\').count();
        let db = b.path.bytes().filter(|&c| c == b'\\').count();
        db.cmp(&da).then_with(|| b.path.len().cmp(&a.path.len()))
    });

    let mut index_map: FxHashMap<String, usize> =
        FxHashMap::with_capacity_and_hasher(dirs.len(), Default::default());
    for (idx, d) in dirs.iter().enumerate() {
        index_map.insert(d.path.clone(), idx);
    }

    for i in 0..dirs.len() {
        let child_total = dirs[i].total_bytes;
        let child_alloc = dirs[i].total_allocated;
        let child_files = dirs[i].file_count;
        if let Some(parent_path) = parent_win_path(&dirs[i].path) {
            if let Some(&p_idx) = index_map.get(parent_path) {
                if p_idx != i {
                    dirs[p_idx].total_bytes += child_total;
                    dirs[p_idx].total_allocated += child_alloc;
                    dirs[p_idx].file_count += child_files;
                }
            }
        }
    }

    let mut top = TopK::new(top_k);
    for d in dirs {
        top.push(FolderEntry {
            total_bytes: d.total_bytes,
            allocated_bytes: d.total_allocated,
            direct_bytes: d.direct_bytes,
            file_count: d.file_count,
            path: d.path,
        });
    }
    top.into_sorted_desc()
}

#[inline]
fn extract_ext(name: &str) -> &str {
    match name.rfind('.') {
        Some(idx) if idx + 1 < name.len() => &name[idx + 1..],
        _ => "",
    }
}

#[inline]
fn join_win_path(dir: &str, name: &str) -> String {
    let mut s = String::with_capacity(dir.len() + 1 + name.len());
    s.push_str(dir);
    if !dir.ends_with('\\') {
        s.push('\\');
    }
    s.push_str(name);
    s
}

#[inline]
fn parent_win_path(path: &str) -> Option<&str> {
    let trimmed = path.trim_end_matches('\\');
    // Don't strip drive root like `C:`
    if trimmed.len() <= 2 && trimmed.ends_with(':') {
        return None;
    }
    let idx = trimmed.rfind('\\')?;
    let parent = &trimmed[..idx];
    if parent.len() == 2 && parent.ends_with(':') {
        // Return `C:\` to match normalized drive root
        Some(&path[..3])
    } else if parent.is_empty() {
        None
    } else {
        Some(parent)
    }
}

fn normalize_root(root: &str) -> String {
    let r = root.trim().replace('/', "\\");
    if r.len() == 2 && r.ends_with(':') {
        format!("{}\\", r)
    } else if r.len() > 3 && r.ends_with('\\') {
        r.trim_end_matches('\\').to_string()
    } else {
        r
    }
}
