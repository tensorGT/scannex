//! rust_scanner - proper fast Windows storage scanner.
//! CLI by default (with interactive tabs), or --ui to pop open the Slint GUI.

mod cli_tui;
mod gui;
mod scanner;
mod types;
mod win_fs;

use std::io::{self, Write};
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use clap::{ArgEnum, Parser};

use crate::cli_tui::{print_plain_report, run_tui};
use crate::gui::launch_gui;
use crate::scanner::{LiveProgress, run_scan};
use crate::types::{ScanMode, format_bytes};

#[derive(Copy, Clone, PartialEq, Eq, ArgEnum)]
enum CliMode {
    Ultrafast,
    Detailed,
}

impl From<CliMode> for ScanMode {
    fn from(m: CliMode) -> Self {
        match m {
            CliMode::Ultrafast => ScanMode::Ultrafast,
            CliMode::Detailed => ScanMode::Detailed,
        }
    }
}

#[derive(Parser)]
#[clap(
    name = "rust_scanner",
    version = "0.1.4",
    about = "Proper fast Windows disk scanner. CLI by default, or --ui for Slint GUI."
)]
struct Cli {
    /// Folder or drive to scan (defaults to C:\)
    #[clap(default_value = "C:\\")]
    path: String,

    /// Scan mode: ultrafast (default) or detailed
    #[clap(long, arg_enum, default_value = "ultrafast")]
    mode: CliMode,

    /// Launch Slint GUI instead of CLI
    #[clap(long)]
    ui: bool,

    /// Number of parallel worker threads (auto-tuned by default)
    #[clap(long)]
    workers: Option<usize>,

    /// How many top files/folders/extensions to keep per tab
    #[clap(long, default_value = "100")]
    top: usize,

    /// Try GPU compute if available (falls back to CPU I/O pool for NTFS syscalls)
    #[clap(long)]
    gpu: bool,

    /// Dump plain text summary to stdout instead of opening interactive CLI tabs
    #[clap(long)]
    no_tui: bool,
}

fn main() {
    let cli = Cli::parse();
    let mode: ScanMode = cli.mode.into();

    if cli.ui {
        launch_gui(cli.path, mode, cli.workers, cli.top, cli.gpu);
        return;
    }

    let progress = LiveProgress::new();
    let prog_watch = progress.clone();

    // Show a clean live status line while scanning in CLI mode
    let spinner_handle = thread::spawn(move || {
        let frames = ['|', '/', '-', '\\'];
        let mut idx = 0usize;
        while !prog_watch.done.load(Ordering::Acquire) {
            let f = prog_watch.files.load(Ordering::Relaxed);
            let d = prog_watch.dirs.load(Ordering::Relaxed);
            let b = prog_watch.bytes.load(Ordering::Relaxed);
            eprint!(
                "\r[{}] Scanning... {} files | {} dirs | {}   ",
                frames[idx % frames.len()],
                f,
                d,
                format_bytes(b)
            );
            let _ = io::stderr().flush();
            idx += 1;
            thread::sleep(Duration::from_millis(80));
        }
        eprint!("\r                                                                       \r");
        let _ = io::stderr().flush();
    });

    let report = run_scan(&cli.path, mode, cli.workers, cli.top, cli.gpu, progress);
    let _ = spinner_handle.join();

    if cli.no_tui {
        print_plain_report(&report);
    } else if let Err(err) = run_tui(&report) {
        eprintln!("TUI failed ({}), falling back to plain output:", err);
        print_plain_report(&report);
    }
}
