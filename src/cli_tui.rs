//! Interactive CLI TUI using tui 0.18 + crossterm 0.23 (proper 2022 stack).
//! Also has a --no-tui plain stdout mode for quick piping or headless checks.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use tui::Terminal;
use tui::backend::CrosstermBackend;
use tui::layout::{Constraint, Direction, Layout};
use tui::style::{Color, Modifier, Style};
use tui::text::{Span, Spans};
use tui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Tabs};

use crate::types::{FileCategory, ScanMode, ScanReport, format_attrs, format_bytes};

pub fn print_plain_report(report: &ScanReport) {
    let files_per_sec = if report.elapsed_secs > 0.0 {
        report.total_files as f64 / report.elapsed_secs
    } else {
        0.0
    };
    println!("================================================================");
    println!(" rust_scanner | {} | mode: {}", report.root, report.mode.label());
    println!(
        " Backend: {} | Time: {:.3}s | Speed: {:.0} files/s",
        report.compute_backend, report.elapsed_secs, files_per_sec
    );
    println!(
        " Total: {} files, {} dirs, {} logical ({} on disk)",
        report.total_files,
        report.total_dirs,
        format_bytes(report.total_logical_bytes),
        format_bytes(report.total_allocated_bytes)
    );
    println!(
        " Skipped junctions: {} | Access denied: {}",
        report.reparse_skipped_count, report.access_denied_count
    );
    println!("================================================================");

    println!("\n[TAB 1: FILE TYPES]");
    for cat in FileCategory::ALL {
        let agg = &report.categories[cat as usize];
        let pct = if report.total_logical_bytes > 0 {
            (agg.logical_bytes as f64 / report.total_logical_bytes as f64) * 100.0
        } else {
            0.0
        };
        println!(
            "  {:<15} {:>10} files | {:>10} ({:>5.1}%)",
            cat.name(),
            agg.count,
            format_bytes(agg.logical_bytes),
            pct
        );
    }

    println!("\n[TAB 2: TOP FILES (HIGHEST TO LOWEST)]");
    for (i, f) in report.top_files.iter().take(15).enumerate() {
        println!(
            "  #{:<3} {:>10} | {}",
            i + 1,
            format_bytes(f.logical_bytes),
            f.path
        );
    }

    println!("\n[TAB 3: TOP FOLDERS (HIGHEST TO LOWEST)]");
    for (i, d) in report.top_folders.iter().take(15).enumerate() {
        println!(
            "  #{:<3} {:>10} ({:>8} files) | {}",
            i + 1,
            format_bytes(d.total_bytes),
            d.file_count,
            d.path
        );
    }

    println!("\n[TAB 4: TOP EXTENSIONS]");
    for (i, e) in report.top_extensions.iter().take(15).enumerate() {
        println!(
            "  #{:<3} .{:<10} {:>10} files | {:>10}",
            i + 1,
            e.ext,
            e.count,
            format_bytes(e.bytes)
        );
    }

    if report.mode == ScanMode::Detailed {
        println!("\n[TAB 5: DEEP DETAILS]");
        println!(
            "  Hidden: {} ({}) | System: {} ({})",
            report.attrs.hidden_count,
            format_bytes(report.attrs.hidden_bytes),
            report.attrs.system_count,
            format_bytes(report.attrs.system_bytes)
        );
        println!(
            "  ReadOnly: {} | Compressed: {} | Sparse: {} | Cloud-Offline: {}",
            report.attrs.readonly_count,
            report.attrs.compressed_count,
            report.attrs.sparse_count,
            report.attrs.offline_cloud_count
        );
    }
}

pub fn run_tui(report: &ScanReport) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut active_tab: usize = 0;
    let tab_count = if report.mode == ScanMode::Detailed { 5 } else { 4 };
    let mut scroll_states: [TableState; 5] = Default::default();
    for st in &mut scroll_states {
        st.select(Some(0));
    }

    loop {
        terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .margin(1)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Min(10),
                    Constraint::Length(2),
                ])
                .split(f.size());

            let speed = if report.elapsed_secs > 0.0 {
                report.total_files as f64 / report.elapsed_secs
            } else {
                0.0
            };
            let header_text = format!(
                " Target: {}  |  Mode: {}  |  {}  |  {:.2}s ({:.0} files/s)  |  Total: {} ({} files)",
                report.root,
                report.mode.label(),
                report.compute_backend,
                report.elapsed_secs,
                speed,
                format_bytes(report.total_logical_bytes),
                report.total_files
            );
            let header = Paragraph::new(header_text)
                .style(Style::default().fg(Color::Cyan))
                .block(Block::default().borders(Borders::ALL).title(" rust_scanner (CLI TUI) "));
            f.render_widget(header, chunks[0]);

            let mut titles = vec![
                Spans::from(Span::raw("1: File Types")),
                Spans::from(Span::raw("2: Top Files")),
                Spans::from(Span::raw("3: Top Folders")),
                Spans::from(Span::raw("4: Extensions & Info")),
            ];
            if report.mode == ScanMode::Detailed {
                titles.push(Spans::from(Span::raw("5: Deep Details")));
            }
            let tabs = Tabs::new(titles)
                .block(Block::default().borders(Borders::ALL).title(" Tabs "))
                .select(active_tab)
                .style(Style::default().fg(Color::White))
                .highlight_style(
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                );
            f.render_widget(tabs, chunks[1]);

            match active_tab {
                0 => render_categories_tab(f, chunks[2], report, &mut scroll_states[0]),
                1 => render_files_tab(f, chunks[2], report, &mut scroll_states[1]),
                2 => render_folders_tab(f, chunks[2], report, &mut scroll_states[2]),
                3 => render_extensions_tab(f, chunks[2], report, &mut scroll_states[3]),
                _ => render_details_tab(f, chunks[2], report),
            }

            let footer = Paragraph::new(
                " Keys: [Tab / Left / Right / 1-5] Switch Tab  |  [Up / Down / PgUp / PgDn] Scroll  |  [q / Esc] Quit ",
            )
            .style(Style::default().fg(Color::DarkGray));
            f.render_widget(footer, chunks[3]);
        })?;

        if event::poll(Duration::from_millis(120))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Tab | KeyCode::Right => {
                        active_tab = (active_tab + 1) % tab_count;
                    }
                    KeyCode::BackTab | KeyCode::Left => {
                        active_tab = if active_tab == 0 { tab_count - 1 } else { active_tab - 1 };
                    }
                    KeyCode::Char('1') => active_tab = 0,
                    KeyCode::Char('2') => active_tab = 1,
                    KeyCode::Char('3') => active_tab = 2,
                    KeyCode::Char('4') => active_tab = 3,
                    KeyCode::Char('5') if tab_count >= 5 => active_tab = 4,
                    KeyCode::Down => move_selection(&mut scroll_states[active_tab], 1, max_rows(report, active_tab)),
                    KeyCode::Up => move_selection(&mut scroll_states[active_tab], -1, max_rows(report, active_tab)),
                    KeyCode::PageDown => move_selection(&mut scroll_states[active_tab], 10, max_rows(report, active_tab)),
                    KeyCode::PageUp => move_selection(&mut scroll_states[active_tab], -10, max_rows(report, active_tab)),
                    _ => {}
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn max_rows(report: &ScanReport, tab: usize) -> usize {
    match tab {
        0 => FileCategory::COUNT,
        1 => report.top_files.len(),
        2 => report.top_folders.len(),
        3 => report.top_extensions.len(),
        _ => 1,
    }
}

fn move_selection(state: &mut TableState, delta: isize, len: usize) {
    if len == 0 {
        return;
    }
    let current = state.selected().unwrap_or(0) as isize;
    let next = (current + delta).clamp(0, (len - 1) as isize) as usize;
    state.select(Some(next));
}

fn render_categories_tab<B: tui::backend::Backend>(
    f: &mut tui::Frame<B>,
    area: tui::layout::Rect,
    report: &ScanReport,
    state: &mut TableState,
) {
    let mut rows_data: Vec<(FileCategory, u64, u64)> = FileCategory::ALL
        .iter()
        .map(|&c| {
            let agg = &report.categories[c as usize];
            (c, agg.count, agg.logical_bytes)
        })
        .collect();
    rows_data.sort_unstable_by(|a, b| b.2.cmp(&a.2));

    let rows: Vec<Row> = rows_data
        .iter()
        .map(|(cat, count, bytes)| {
            let pct = if report.total_logical_bytes > 0 {
                (*bytes as f64 / report.total_logical_bytes as f64) * 100.0
            } else {
                0.0
            };
            let bars = ((pct / 5.0).round() as usize).min(20);
            let bar_str = format!("{}{}", "#".repeat(bars), ".".repeat(20 - bars));
            Row::new(vec![
                Cell::from(cat.name()),
                Cell::from(format!("{}", count)),
                Cell::from(format_bytes(*bytes)),
                Cell::from(format!("{:.1}%", pct)),
                Cell::from(bar_str),
            ])
        })
        .collect();

    let table = Table::new(rows)
        .header(
            Row::new(vec!["Category", "Files", "Total Size", "Share", "Usage Bar"])
                .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        )
        .block(Block::default().borders(Borders::ALL).title(" Storage by File Type "))
        .highlight_style(Style::default().bg(Color::DarkGray))
        .widths(&[
            Constraint::Length(18),
            Constraint::Length(12),
            Constraint::Length(14),
            Constraint::Length(10),
            Constraint::Min(22),
        ]);
    f.render_stateful_widget(table, area, state);
}

fn render_files_tab<B: tui::backend::Backend>(
    f: &mut tui::Frame<B>,
    area: tui::layout::Rect,
    report: &ScanReport,
    state: &mut TableState,
) {
    let rows: Vec<Row> = report
        .top_files
        .iter()
        .enumerate()
        .map(|(i, file)| {
            if report.mode == ScanMode::Detailed {
                Row::new(vec![
                    Cell::from(format!("#{}", i + 1)),
                    Cell::from(format_bytes(file.logical_bytes)),
                    Cell::from(format_bytes(file.allocated_bytes)),
                    Cell::from(format_attrs(file.attrs)),
                    Cell::from(file.path.as_str()),
                ])
            } else {
                Row::new(vec![
                    Cell::from(format!("#{}", i + 1)),
                    Cell::from(format_bytes(file.logical_bytes)),
                    Cell::from(file.path.as_str()),
                ])
            }
        })
        .collect();

    let table = if report.mode == ScanMode::Detailed {
        Table::new(rows)
            .header(
                Row::new(vec!["Rank", "Logical", "On Disk", "Attrs", "Path"])
                    .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            )
            .widths(&[
                Constraint::Length(6),
                Constraint::Length(12),
                Constraint::Length(12),
                Constraint::Length(8),
                Constraint::Min(30),
            ])
    } else {
        Table::new(rows)
            .header(
                Row::new(vec!["Rank", "Size", "Path"])
                    .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            )
            .widths(&[
                Constraint::Length(6),
                Constraint::Length(14),
                Constraint::Min(30),
            ])
    }
    .block(Block::default().borders(Borders::ALL).title(" Largest Files (Highest -> Lowest) "))
    .highlight_style(Style::default().bg(Color::DarkGray));

    f.render_stateful_widget(table, area, state);
}

fn render_folders_tab<B: tui::backend::Backend>(
    f: &mut tui::Frame<B>,
    area: tui::layout::Rect,
    report: &ScanReport,
    state: &mut TableState,
) {
    let rows: Vec<Row> = report
        .top_folders
        .iter()
        .enumerate()
        .map(|(i, dir)| {
            Row::new(vec![
                Cell::from(format!("#{}", i + 1)),
                Cell::from(format_bytes(dir.total_bytes)),
                Cell::from(format!("{}", dir.file_count)),
                Cell::from(format_bytes(dir.direct_bytes)),
                Cell::from(dir.path.as_str()),
            ])
        })
        .collect();

    let table = Table::new(rows)
        .header(
            Row::new(vec!["Rank", "Total Usage", "Files", "Direct Files", "Folder Path"])
                .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        )
        .block(Block::default().borders(Borders::ALL).title(" Heaviest Folders (Highest -> Lowest) "))
        .highlight_style(Style::default().bg(Color::DarkGray))
        .widths(&[
            Constraint::Length(6),
            Constraint::Length(14),
            Constraint::Length(10),
            Constraint::Length(14),
            Constraint::Min(30),
        ]);
    f.render_stateful_widget(table, area, state);
}

fn render_extensions_tab<B: tui::backend::Backend>(
    f: &mut tui::Frame<B>,
    area: tui::layout::Rect,
    report: &ScanReport,
    state: &mut TableState,
) {
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);

    let rows: Vec<Row> = report
        .top_extensions
        .iter()
        .enumerate()
        .map(|(i, e)| {
            Row::new(vec![
                Cell::from(format!("#{}", i + 1)),
                Cell::from(format!(".{}", e.ext)),
                Cell::from(format!("{}", e.count)),
                Cell::from(format_bytes(e.bytes)),
            ])
        })
        .collect();

    let table = Table::new(rows)
        .header(
            Row::new(vec!["Rank", "Ext", "Files", "Total Size"])
                .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        )
        .block(Block::default().borders(Borders::ALL).title(" Top Extensions "))
        .highlight_style(Style::default().bg(Color::DarkGray))
        .widths(&[
            Constraint::Length(6),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Min(12),
        ]);
    f.render_stateful_widget(table, split[0], state);

    let vol_text = if let Some(v) = &report.volume {
        format!(
            "Volume: {} ({})\nDrive Total: {}\nDrive Free: {}\n\nDirectories: {}\nFiles: {}\nJunctions Skipped: {}\nAccess Denied: {}",
            v.volume_name,
            v.fs_name,
            format_bytes(v.total_bytes),
            format_bytes(v.free_bytes),
            report.total_dirs,
            report.total_files,
            report.reparse_skipped_count,
            report.access_denied_count
        )
    } else {
        format!(
            "Directories: {}\nFiles: {}\nJunctions Skipped: {}\nAccess Denied: {}",
            report.total_dirs,
            report.total_files,
            report.reparse_skipped_count,
            report.access_denied_count
        )
    };

    let info = Paragraph::new(vol_text)
        .block(Block::default().borders(Borders::ALL).title(" Volume & Scan Telemetry "));
    f.render_widget(info, split[1]);
}

fn render_details_tab<B: tui::backend::Backend>(
    f: &mut tui::Frame<B>,
    area: tui::layout::Rect,
    report: &ScanReport,
) {
    let saved = report
        .total_logical_bytes
        .saturating_sub(report.total_allocated_bytes);
    let text = format!(
        "Detailed Storage Semantics (Windows NTFS)\n\
         ------------------------------------------\n\
         Logical Size Total   : {}\n\
         Allocated Size Total : {}\n\
         Compression/Sparse   : {} saved on disk\n\n\
         File Attributes Breakdown\n\
         ------------------------------------------\n\
         Hidden Files         : {} ({})\n\
         System Files         : {} ({})\n\
         Read-Only Files      : {}\n\
         NTFS Compressed      : {}\n\
         Sparse Files         : {}\n\
         Cloud / Offline      : {}\n",
        format_bytes(report.total_logical_bytes),
        format_bytes(report.total_allocated_bytes),
        format_bytes(saved),
        report.attrs.hidden_count,
        format_bytes(report.attrs.hidden_bytes),
        report.attrs.system_count,
        format_bytes(report.attrs.system_bytes),
        report.attrs.readonly_count,
        report.attrs.compressed_count,
        report.attrs.sparse_count,
        report.attrs.offline_cloud_count,
    );
    let p = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title(" Deep Windows Details "));
    f.render_widget(p, area);
}
