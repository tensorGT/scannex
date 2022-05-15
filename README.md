# rust_scanner

Proper fast Windows storage scanner written in Rust. Built in one week in May 2022 because I got tired of waiting 20 minutes just to see what is eating my C: drive.

Runs as a **CLI with interactive tabs by default**, or pass `--ui` if you want the **Slint desktop GUI**. Zero prerequisites — just one standalone `.exe` you can drop anywhere and run.

---

## Quick Start (No Stress)

```powershell
# Ultrafast CLI mode (default - opens interactive tabs in your terminal)
rust_scanner.exe C:\

# Ultrafast mode, plain stdout (no TUI, good for quick checks)
rust_scanner.exe C:\Users --no-tui

# Detailed mode in CLI (slower, still super fast - grabs on-disk allocated size & file attributes)
rust_scanner.exe C:\ --mode detailed

# Launch the Slint GUI (works in both ultrafast and detailed!)
rust_scanner.exe C:\ --ui
rust_scanner.exe C:\ --mode detailed --ui
```

### Modes & Tabs
1. **Ultrafast Mode (`--mode ultrafast`, default)**
   - **Tab 1 — File Types:** Total files & size grouped by Video, Audio, Images, Archives, Code & Config, Apps & Bins, Documents, and Other Stuff.
   - **Tab 2 — Top Files:** Largest files sorted highest to lowest.
   - **Tab 3 — Top Folders:** Heaviest folders sorted highest to lowest (recursive usage).
   - **Tab 4 — Extensions & Info:** Top file extensions + volume info, scan speed, skipped junctions, and access-denied count.
2. **Detailed Mode (`--mode detailed`)**
   - Enriches Top Files with actual on-disk allocated size (`GetCompressedFileSizeW`) and Windows attribute flags (`RHSACO`).
   - Adds **Tab 5 — Deep Details:** Logical vs Allocated size, NTFS compression/sparse savings, and Hidden / System / Read-Only / Cloud-Offline breakdowns.

---

## How It's Built Under the Hood

### 1. Windows Filesystem & Storage Semantics
- **Direct Win32 `FindFirstFileExW`**: `std::fs::read_dir` is nice and all, but it leaves serious speed on the table. We call `FindFirstFileExW` directly with `FindExInfoBasic` (tells Windows to skip generating/looking up 8.3 DOS short filenames) and `FIND_FIRST_EX_LARGE_FETCH` (bigger kernel directory buffers, way fewer syscalls on massive folders like `WinSxS` or `node_modules`).
- **Extended-Length Paths (`\\?\`)**: Every path is converted to UTF-16 with the `\\?\` prefix so paths deeper than 260 characters (`MAX_PATH`) never fail.
- **Reparse Point Safety**: Windows has NTFS junctions everywhere (`C:\Users\All Users`, `AppData\Local\Application Data`). We check `FILE_ATTRIBUTE_REPARSE_POINT` and skip recursing into them so we never double-count or loop forever.
- **Logical vs Allocated Size**: In `detailed` mode, we query `GetCompressedFileSizeW` so you can see how much space compressed or sparse files actually take on disk.

### 2. High-Throughput Concurrency & I/O Scheduling
- **Lock-Free MPMC Work Queue**: Workers pull directories from an unbounded `crossbeam-channel` and push subdirectories right back in. When the active directory counter hits zero, poison pills wake everyone up cleanly.
- **Zero-Contention Thread-Local Accumulators**: While walking folders, workers **never** touch a shared mutex. Each worker keeps its own category totals, `FxHashMap` extension table, and `TopK` heaps, then merges once at the end.
- **GPU vs CPU Reality Check**: You can pass `--gpu`, and we'll check the backend, but let's not lie to each other — Windows NTFS metadata syscalls (`FindFirstFileExW`) have to go through the OS kernel on CPU anyway. So when no GPU compute offload applies, we automatically spin up a tuned CPU I/O worker pool (`2x` logical cores, clamped `4..=32`) to keep NVMe/SSD command queues saturated.

### 3. Memory Layout & Zero-Cost Rust Discipline
- **Compact `#[repr(C)]` Structs**: `CategoryAgg` and `AttrBreakdown` stay tight and cache-friendly.
- **Reusable UTF-16 Scratch Buffers**: Each worker reuses three `Vec<u16>` buffers across millions of files instead of allocating on every entry.
- **Bounded `TopK` Min-Heaps**: Scanning 5 million files won't blow up your RAM because each worker only keeps the Top K heaviest files/folders in a `BinaryHeap<Reverse<T>>`.

### 4. Production Reliability & UX Engineering
- **No Panics on Locked Folders**: `ERROR_ACCESS_DENIED` on system folders like `System Volume Information` is caught, counted in telemetry, and skipped cleanly.
- **CLI First, Slint Optional**: Interactive `tui` + `crossterm` tabs in the terminal (`Tab` / `1-5` / Arrows / `q`), `--no-tui` for scripts, or `--ui` for Slint desktop mode — all from one zero-dependency binary.

---

## Building from Source

```powershell
cargo build --release
```
Binary lands at `target\release\rust_scanner.exe`.
