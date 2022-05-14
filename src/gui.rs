//! Slint GUI launcher. Works in both Ultrafast and Detailed modes.
//! Spawns the scanner on a background thread so the window never freezes.

use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use slint::{ModelRc, SharedString, StandardListViewItem, VecModel};

use crate::scanner::{LiveProgress, run_scan};
use crate::types::{FileCategory, ScanMode, ScanReport, format_attrs, format_bytes};

slint::include_modules!();

pub fn launch_gui(initial_path: String, initial_mode: ScanMode, workers: Option<usize>, top_k: usize, prefer_gpu: bool) {
    let ui = AppWindow::new().expect("failed to create Slint window");
    ui.set_target_path(SharedString::from(initial_path.clone()));
    ui.set_is_detailed(initial_mode == ScanMode::Detailed);

    let ui_handle = ui.as_weak();
    let trigger_scan = move |path: String, detailed: bool| {
        let mode = if detailed {
            ScanMode::Detailed
        } else {
            ScanMode::Ultrafast
        };

        if let Some(u) = ui_handle.upgrade() {
            u.set_is_scanning(true);
            u.set_status_text(SharedString::from(format!(
                "Scanning {} in {} mode...",
                path,
                mode.label()
            )));
        }

        let ui_weak_prog = ui_handle.clone();
        let ui_weak_done = ui_handle.clone();
        let progress = LiveProgress::new();
        let prog_poll = progress.clone();

        // Live progress updater thread
        thread::spawn(move || {
            while !prog_poll.done.load(Ordering::Acquire) {
                let f = prog_poll.files.load(Ordering::Relaxed);
                let d = prog_poll.dirs.load(Ordering::Relaxed);
                let b = prog_poll.bytes.load(Ordering::Relaxed);
                let w = ui_weak_prog.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(u) = w.upgrade() {
                        u.set_status_text(SharedString::from(format!(
                            "Scanning... {} files | {} dirs | {}",
                            f,
                            d,
                            format_bytes(b)
                        )));
                    }
                });
                thread::sleep(Duration::from_millis(120));
            }
        });

        thread::spawn(move || {
            let report = run_scan(&path, mode, workers, top_k, prefer_gpu, progress);
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(u) = ui_weak_done.upgrade() {
                    apply_report_to_ui(&u, &report);
                    u.set_is_scanning(false);
                }
            });
        });
    };

    let trigger_cb = trigger_scan.clone();
    ui.on_start_scan(move |path, detailed| {
        trigger_cb(path.to_string(), detailed);
    });

    // Kick off initial scan automatically when launched with --ui
    trigger_scan(initial_path, initial_mode == ScanMode::Detailed);
    let _ = ui.run();
}

fn to_model(items: Vec<String>) -> ModelRc<StandardListViewItem> {
    let list: Vec<StandardListViewItem> = items
        .into_iter()
        .map(|s| StandardListViewItem::from(SharedString::from(s)))
        .collect();
    ModelRc::from(Rc::new(VecModel::from(list)))
}

fn apply_report_to_ui(ui: &AppWindow, report: &ScanReport) {
    let speed = if report.elapsed_secs > 0.0 {
        report.total_files as f64 / report.elapsed_secs
    } else {
        0.0
    };
    ui.set_status_text(SharedString::from(format!(
        "Done in {:.2}s ({:.0} files/s) | {} | {} files, {} dirs | {} | Skipped junctions: {}, Denied: {}",
        report.elapsed_secs,
        speed,
        report.compute_backend,
        report.total_files,
        report.total_dirs,
        format_bytes(report.total_logical_bytes),
        report.reparse_skipped_count,
        report.access_denied_count
    )));

    // Tab 1: File Types
    let mut cats: Vec<(FileCategory, u64, u64)> = FileCategory::ALL
        .iter()
        .map(|&c| {
            let agg = &report.categories[c as usize];
            (c, agg.count, agg.logical_bytes)
        })
        .collect();
    cats.sort_unstable_by(|a, b| b.2.cmp(&a.2));
    let cat_lines = cats
        .into_iter()
        .map(|(cat, count, bytes)| {
            let pct = if report.total_logical_bytes > 0 {
                (bytes as f64 / report.total_logical_bytes as f64) * 100.0
            } else {
                0.0
            };
            format!(
                "{:<16} | {:>10} files | {:>11} ({:>5.1}%)",
                cat.name(),
                count,
                format_bytes(bytes),
                pct
            )
        })
        .collect();
    ui.set_category_items(to_model(cat_lines));

    // Tab 2: Top Files
    let file_lines = report
        .top_files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            if report.mode == ScanMode::Detailed {
                format!(
                    "#{:<3} | {:>11} (disk {:>11}) | {} | {}",
                    i + 1,
                    format_bytes(f.logical_bytes),
                    format_bytes(f.allocated_bytes),
                    format_attrs(f.attrs),
                    f.path
                )
            } else {
                format!("#{:<3} | {:>11} | {}", i + 1, format_bytes(f.logical_bytes), f.path)
            }
        })
        .collect();
    ui.set_file_items(to_model(file_lines));

    // Tab 3: Top Folders
    let folder_lines = report
        .top_folders
        .iter()
        .enumerate()
        .map(|(i, d)| {
            format!(
                "#{:<3} | {:>11} ({:>8} files) | {}",
                i + 1,
                format_bytes(d.total_bytes),
                d.file_count,
                d.path
            )
        })
        .collect();
    ui.set_folder_items(to_model(folder_lines));

    // Tab 4: Extensions & Volume
    let mut ext_lines: Vec<String> = Vec::new();
    if let Some(v) = &report.volume {
        ext_lines.push(format!(
            "Volume: {} ({}) | Total: {} | Free: {}",
            v.volume_name,
            v.fs_name,
            format_bytes(v.total_bytes),
            format_bytes(v.free_bytes)
        ));
    }
    for (i, e) in report.top_extensions.iter().enumerate() {
        ext_lines.push(format!(
            "#{:<3} | .{:<10} | {:>10} files | {:>11}",
            i + 1,
            e.ext,
            e.count,
            format_bytes(e.bytes)
        ));
    }
    ui.set_ext_items(to_model(ext_lines));

    // Tab 5: Deep Details
    let detail_lines = if report.mode == ScanMode::Detailed {
        let saved = report
            .total_logical_bytes
            .saturating_sub(report.total_allocated_bytes);
        vec![
            format!("Logical Size Total   : {}", format_bytes(report.total_logical_bytes)),
            format!("Allocated Size Total : {}", format_bytes(report.total_allocated_bytes)),
            format!("Compression/Sparse   : {} saved", format_bytes(saved)),
            format!(
                "Hidden Files         : {} ({})",
                report.attrs.hidden_count,
                format_bytes(report.attrs.hidden_bytes)
            ),
            format!(
                "System Files         : {} ({})",
                report.attrs.system_count,
                format_bytes(report.attrs.system_bytes)
            ),
            format!("Read-Only Files      : {}", report.attrs.readonly_count),
            format!("NTFS Compressed      : {}", report.attrs.compressed_count),
            format!("Sparse Files         : {}", report.attrs.sparse_count),
            format!("Cloud / Offline      : {}", report.attrs.offline_cloud_count),
        ]
    } else {
        vec![
            "You ran in Ultrafast mode! Switch to Mode: Detailed and hit Scan Now for allocated disk sizes, attributes, and compression stats.".to_string(),
        ]
    };
    ui.set_detail_items(to_model(detail_lines));
}
