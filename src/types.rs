//! Core data types and zero-allocation helpers.
//! Everything here is kept tight in memory so even if you scan a whole 2TB drive
//! with millions of files, your RAM won't start crying.

use rustc_hash::FxHashMap;
use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanMode {
    Ultrafast,
    Detailed,
}

impl ScanMode {
    pub fn label(self) -> &'static str {
        match self {
            ScanMode::Ultrafast => "ultrafast",
            ScanMode::Detailed => "detailed",
        }
    }
}

/// Broad file buckets. Keeps Tab 1 clean and readable without 500 random extensions.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileCategory {
    Video = 0,
    Audio = 1,
    Images = 2,
    Archives = 3,
    Code = 4,
    Executables = 5,
    Documents = 6,
    Other = 7,
}

impl FileCategory {
    pub const COUNT: usize = 8;

    pub const ALL: [FileCategory; Self::COUNT] = [
        FileCategory::Video,
        FileCategory::Audio,
        FileCategory::Images,
        FileCategory::Archives,
        FileCategory::Code,
        FileCategory::Executables,
        FileCategory::Documents,
        FileCategory::Other,
    ];

    pub fn name(self) -> &'static str {
        match self {
            FileCategory::Video => "Video",
            FileCategory::Audio => "Audio",
            FileCategory::Images => "Images",
            FileCategory::Archives => "Archives",
            FileCategory::Code => "Code & Config",
            FileCategory::Executables => "Apps & Bins",
            FileCategory::Documents => "Documents",
            FileCategory::Other => "Other Stuff",
        }
    }
}

/// Classify extension bytes without allocating a String on the heap.
/// Pure slice matching - fast as lightning, zero stress.
#[inline]
pub fn classify_ext(ext: &str) -> FileCategory {
    let bytes = ext.as_bytes();
    if bytes.is_empty() || bytes.len() > 12 {
        return FileCategory::Other;
    }

    let mut buf = [0u8; 12];
    for (i, &b) in bytes.iter().enumerate() {
        buf[i] = b.to_ascii_lowercase();
    }
    let lower = &buf[..bytes.len()];

    match lower {
        b"mp4" | b"mkv" | b"avi" | b"mov" | b"wmv" | b"flv" | b"webm" | b"m4v" => {
            FileCategory::Video
        }
        b"mp3" | b"wav" | b"flac" | b"aac" | b"ogg" | b"m4a" | b"wma" | b"opus" => {
            FileCategory::Audio
        }
        b"jpg" | b"jpeg" | b"png" | b"gif" | b"webp" | b"bmp" | b"svg" | b"ico" | b"tiff"
        | b"raw" | b"heic" | b"psd" => FileCategory::Images,
        b"zip" | b"rar" | b"7z" | b"tar" | b"gz" | b"bz2" | b"xz" | b"iso" | b"zst"
        | b"cab" => FileCategory::Archives,
        b"rs" | b"py" | b"js" | b"ts" | b"tsx" | b"jsx" | b"c" | b"cpp" | b"h" | b"hpp"
        | b"go" | b"java" | b"cs" | b"json" | b"toml" | b"yaml" | b"yml" | b"xml" | b"html"
        | b"css" | b"sql" | b"sh" | b"ps1" | b"bat" | b"md" => FileCategory::Code,
        b"exe" | b"dll" | b"msi" | b"sys" | b"pdb" | b"lib" | b"so" | b"apk" | b"appx" => {
            FileCategory::Executables
        }
        b"pdf" | b"doc" | b"docx" | b"xls" | b"xlsx" | b"ppt" | b"pptx" | b"txt" | b"csv"
        | b"rtf" | b"odt" | b"epub" => FileCategory::Documents,
        _ => FileCategory::Other,
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CategoryAgg {
    pub count: u64,
    pub logical_bytes: u64,
    pub allocated_bytes: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct AttrBreakdown {
    pub hidden_count: u64,
    pub hidden_bytes: u64,
    pub system_count: u64,
    pub system_bytes: u64,
    pub readonly_count: u64,
    pub compressed_count: u64,
    pub sparse_count: u64,
    pub offline_cloud_count: u64,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct FileEntry {
    pub logical_bytes: u64,
    pub allocated_bytes: u64,
    pub modified_unix: i64,
    pub attrs: u32,
    pub path: String,
}

impl Ord for FileEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.logical_bytes
            .cmp(&other.logical_bytes)
            .then_with(|| self.path.cmp(&other.path))
    }
}

impl PartialOrd for FileEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct FolderEntry {
    pub total_bytes: u64,
    pub allocated_bytes: u64,
    pub direct_bytes: u64,
    pub file_count: u64,
    pub path: String,
}

impl Ord for FolderEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.total_bytes
            .cmp(&other.total_bytes)
            .then_with(|| self.path.cmp(&other.path))
    }
}

impl PartialOrd for FolderEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Bounded min-heap so we only keep the Top K heaviest files/folders in RAM.
/// Scanning 5 million files? No problem, we only hold e.g. 100 items per worker.
pub struct TopK<T: Ord> {
    cap: usize,
    heap: BinaryHeap<Reverse<T>>,
}

impl<T: Ord> TopK<T> {
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            heap: BinaryHeap::with_capacity(cap.max(1) + 1),
        }
    }

    #[inline]
    pub fn push(&mut self, item: T) {
        if self.heap.len() < self.cap {
            self.heap.push(Reverse(item));
        } else if let Some(mut smallest) = self.heap.peek_mut() {
            if item > smallest.0 {
                smallest.0 = item;
            }
        }
    }

    pub fn into_sorted_desc(self) -> Vec<T> {
        let mut items: Vec<T> = self.heap.into_iter().map(|r| r.0).collect();
        items.sort_unstable_by(|a, b| b.cmp(a));
        items
    }
}

#[derive(Debug, Clone, Default)]
pub struct VolumeInfo {
    pub volume_name: String,
    pub fs_name: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct ExtStat {
    pub ext: String,
    pub count: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub struct ScanReport {
    pub root: String,
    pub mode: ScanMode,
    pub workers_used: usize,
    pub compute_backend: String,
    pub elapsed_secs: f64,
    pub total_files: u64,
    pub total_dirs: u64,
    pub total_logical_bytes: u64,
    pub total_allocated_bytes: u64,
    pub access_denied_count: u64,
    pub reparse_skipped_count: u64,
    pub categories: [CategoryAgg; FileCategory::COUNT],
    pub top_files: Vec<FileEntry>,
    pub top_folders: Vec<FolderEntry>,
    pub top_extensions: Vec<ExtStat>,
    pub attrs: AttrBreakdown,
    pub volume: Option<VolumeInfo>,
}

/// Per-worker accumulator. Zero mutexes touched while walking directories!
pub struct WorkerLocal {
    pub total_files: u64,
    pub total_dirs: u64,
    pub total_logical_bytes: u64,
    pub total_allocated_bytes: u64,
    pub access_denied_count: u64,
    pub reparse_skipped_count: u64,
    pub categories: [CategoryAgg; FileCategory::COUNT],
    pub extensions: FxHashMap<String, CategoryAgg>,
    pub top_files: TopK<FileEntry>,
    pub top_folders: TopK<FolderEntry>,
    pub attrs: AttrBreakdown,
}

impl WorkerLocal {
    pub fn new(top_k: usize) -> Self {
        Self {
            total_files: 0,
            total_dirs: 0,
            total_logical_bytes: 0,
            total_allocated_bytes: 0,
            access_denied_count: 0,
            reparse_skipped_count: 0,
            categories: [CategoryAgg::default(); FileCategory::COUNT],
            extensions: FxHashMap::default(),
            top_files: TopK::new(top_k),
            top_folders: TopK::new(top_k),
            attrs: AttrBreakdown::default(),
        }
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut val = bytes as f64;
    let mut idx = 0;
    while val >= 1024.0 && idx < UNITS.len() - 1 {
        val /= 1024.0;
        idx += 1;
    }
    if idx == 0 {
        format!("{} B", bytes)
    } else {
        format!("{:.2} {}", val, UNITS[idx])
    }
}

pub fn format_attrs(attrs: u32) -> String {
    let mut s = String::with_capacity(6);
    s.push(if attrs & 0x01 != 0 { 'R' } else { '-' });
    s.push(if attrs & 0x02 != 0 { 'H' } else { '-' });
    s.push(if attrs & 0x04 != 0 { 'S' } else { '-' });
    s.push(if attrs & 0x20 != 0 { 'A' } else { '-' });
    s.push(if attrs & 0x800 != 0 { 'C' } else { '-' });
    s.push(if attrs & 0x1000 != 0 { 'O' } else { '-' });
    s
}
