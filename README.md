# sd2img

[English](README.md) | [日本語](README.ja.md)

A Windows command-line tool for imaging SD cards with read errors or intermittent disconnects. The Rust package and executable are named `sd2img`.

The tool reads a physical disk into a raw image and tracks progress in a companion map file so that interrupted sessions can be resumed. Application messages follow the Windows user UI language: Japanese for Japanese Windows, English otherwise.

## Features

- Start coarse reads at 64 KiB and tune their size using measured time per byte; revisit failed ranges one logical sector at a time.
- Choose forward, reverse, or adaptive random read order (random by default). Random scans favor unexplored neighbors of fast successful runs, continue in the same direction after success, and fall back to a random untried location when no neighbor candidate remains.
- Explore other regions after a configurable scheduling delay without cancelling pending reads, with a live full-screen map and animated device-wait status.
- At scan saturation, verify a known-good sector and decide whether recovery is needed; reacquire volume protection before resuming after reconnection.
- Checkpoint image bytes before saving the map, periodically and on Ctrl+C or Ctrl+Break.
- Append successful reads, recovery attempts, and detailed messages to `<OUTPUT>.log`.
- Attempt Configuration Manager recovery, then SetupAPI recovery, before suggesting a physical reconnect.
- Automatically select a single eligible SD, MMC, or USB disk, or select a disk explicitly.
- Lock and dismount all discovered source volumes, including volumes without drive letters, for the duration of imaging.

## Requirements

- Windows. Disk discovery, volume locking, and UAC elevation use native Windows APIs; the application does not launch PowerShell.
- A current stable Rust toolchain and the native build tools required by your Rust Windows target.
- Administrator privileges to read physical disks. The application requests elevation through UAC when needed.
- A separate destination disk with free space for an image of the entire source disk.

Linux and macOS are not supported. Logical sectors are queried through Windows APIs. Power-of-two sizes from 512 bytes to 64 KiB are supported; unsupported or inconsistent geometry is rejected before imaging.

## Build

From the repository directory in PowerShell:

```powershell
cargo build --release --locked
.\target\release\sd2img.exe --help
```

## Usage

Identify the source disk first using Windows Disk Management, or optionally this PowerShell command:

```powershell
Get-Disk | Format-Table Number, FriendlyName, BusType, Size, IsBoot, IsSystem
```

For example, image physical disk **2** to a different disk, replacing both the disk number and destination with your own values:

```powershell
.\target\release\sd2img.exe -d 2 "D:\Recovery\card.img"
```

Create the destination directory beforehand. A drive letter such as `-d E:` resolves to the **entire physical disk** containing that volume, not just the partition.

To use automatic selection:

```powershell
.\target\release\sd2img.exe "D:\Recovery\card.img"
```

Automatic selection uses native device enumeration and considers SD/MMC/USB disks. It excludes the Windows system disk and, conservatively, disks with EFI system partitions or active MBR boot partitions. Disks whose metadata or partition layout cannot be verified are skipped. It refuses to select if there are zero or multiple candidates. Explicit selection does not apply those discovery filters, but volume locking still applies. Always verify the source yourself.

### Options

| Argument | Default | Description |
| --- | --- | --- |
| `[OUTPUT]` | `sd_backup.img` | Destination image path, specified as a positional argument. |
| `-d`, `--drive` | `auto` | Physical disk number, drive letter, or `auto`. |
| `--direction` | `random` | `forward`, `reverse`, or adaptive `random`; applies to both scan stages. |
| `-t`, `--timeout` | `2` | Initial/minimum read timeout in seconds; automatically scales with successful transfer time and request size. The original read remains pending. |
| `--operation-timeout` | `10` | Slow-operation/reset observation interval and fallback reference-check timeout, in seconds; greater than 0 and at most 3600. |
| `--max-parallel-reads` | `16` | Outstanding scan limit (1–64), plus one separate reference-check slot. |
| `--max-read-size` | `1GiB` | Adaptive request ceiling, e.g. `4MiB` or `1GiB`; not an upfront allocation. |
| `-r`, `--retry-bad` | Disabled | Rescan ranges marked `-` in the map. |
| `-h`, `--help` | | Display help. |
| `-V`, `--version` | | Display version. |

```powershell
.\target\release\sd2img.exe -d E: --direction reverse --timeout 1.5 "D:\Recovery\card.img"
```

### Full-screen display and log

The full-screen display is enabled by default.

- A fixed header shows the source, destination, capacity, recovered/failed/untried counts, current range, state, OK/NG response times and one row of normal-success versus delayed-success transfer rates, each showing mean and median.
- The central map adapts to the terminal dimensions. Each cell represents an automatically calculated number of sectors, covering the complete disk including its last sector.
- The bottom **five rows** show recent warnings, reset attempts, and errors. Long messages wrap within that area.
- Gray `.` means all untried, green `#` all recovered, bright red `X` all failed/awaiting retry, red `x` partly failed/awaiting retry, and blue `:` recovered mixed with untried. `X` does not mean permanently unrecoverable. `R` marks reading and `W` waiting; current activity overrides the stored status.
- The current read remains `R` until its displayed timeout, then becomes `W` if still incomplete. A current `R` takes precedence over older pending requests in the same coarse map cell. The rendering thread remains animated while the scanning thread waits, and never reads or writes storage devices.

Use a VT-capable Windows terminal; 80 columns or wider is recommended (minimum 45 columns and 20 rows). Resizing recalculates the map. `NO_COLOR` disables colors while keeping state symbols. With redirected stdout, imaging runs without a full-screen map and writes its detailed events to the log.

Startup settings and build identifier, request IDs and purposes, applied timeouts and durations, successful reads, frontier choices, errors, and reset results are appended to `<OUTPUT>.log` with local timestamps. Existing logs are retained across sessions. Sector counts include a scaled binary size and exact byte count, such as `128 sectors / 64.00 KiB (65536 bytes)`. The log has no terminal color codes. Checkpoint saves run independently of display refreshes.

### Adaptive random scans and fast neighbors

Fast successful runs create candidate frontiers on both sides. Reads continue in the current direction while successful; a timeout or failure discards that edge. When selecting a fresh target, discard candidates that are no longer untried, then randomly choose among the remaining frontiers. Without candidates, choose randomly over all untried ranges, without filesystem-metadata weighting.

For example, after 100–131 succeeds and 132 stalls, the region before 100 (such as 84–99) is preferred. Success then continues backward to 68–83. Late success only saves the delayed request's data; it does not add neighbor candidates or resume its stream. Explicit `forward` and `reverse` orders remain unchanged.

At most 1,024 frontier candidates are held in memory; oldest entries are evicted at the limit. The map format does not change and the candidate history resets on restart. `[scan target]` logs the source (`fast-neighbor` or `random`) and direction.

### Sector sizes and adaptive request sizes

Use `--max-parallel-reads 16` to allow up to 16 outstanding scan requests (default 16, range 1–64). Scan timeouts proceed to another region without reference checks. At saturation, one separate reference slot allows verification and a reset decision: at most 16 scan requests plus one reference request by default.

Each logical sequential stream retains its own size and timeout calibration, with separate normal and loaded profiles. A timed-out stream ends, keeping only its outstanding I/O request alive. Delayed success saves that request's data and marks it rescued, without resuming or prioritizing its contiguous successor. The current successful stream continues unaffected. Calibration is in memory and restarts with the process.

Failed CM and SetupAPI resets have independent retry cooldowns of 10, 20, 40, 80, 160, then 300 seconds (capped at 300). Reference and reconnection checks continue during cooldown, without closing the source handle just to wait.

Logs include the build identifier, startup settings, request IDs and purposes, byte counts, applied timeouts, elapsed times, pending counts, stream IDs, reference-LBA updates and bytes rescued since the previous recovery attempt. Delayed completion timing measures elapsed time until completion is observed.

Windows [`StorageAccessAlignmentProperty`](https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ns-winioctl-storage_access_alignment_descriptor) supplies the logical sector size, with disk geometry as a fallback. Geometry is cached per opened device and checked again on reconnection, not queried for each read.

New maps contain `SECTOR_SIZE <bytes>`. A map without this field is always interpreted as using **512-byte sectors**, never guessed from its contents. The map's sector size must match the device API result; mismatches stop the run without reinterpreting existing LBAs. Reference-sector data, offsets, size displays, and recovery helpers use that same unit.

The coarse-read tuner starts at **64 KiB**. It passively samples successful, full requests using the I/O completion time per byte, excluding log and image-write time. After seven samples, their median is compared with the previous size. Requests double while efficiency improves by more than 5%; when it does not, the smaller size is retained for the rest of that contiguous run. Each scan read timeout is the latest successful scan duration multiplied by the next request size divided by the successful request size, rounded up to milliseconds, with `--timeout` as a floor. A 64 KiB read taking one second predicts two seconds for 128 KiB and four seconds for 256 KiB. Without a successful sample, the initial timeout applies. Failed reads do not train the estimate.

An error or a noncontiguous region resets tuning to 64 KiB. A truncated tail block does not count as a full calibration sample. Fine scanning continues to read exactly one logical sector at a time. Buffers grow only when required and are trimmed after errors, smaller-size selection, and the transition to fine scanning.

The default **1 GiB** is only a ceiling. It does not mean 1 GiB is allocated or read immediately, nor that the tuner will reach it. Configure a smaller ceiling if desired:

```powershell
.\target\release\sd2img.exe -d E: --max-read-size 4MiB "D:\Recovery\card.img"
```

Accepted units are bytes, `KiB`/`MiB`/`GiB`, or decimal `KB`/`MB`/`GB`. Values must be integers between 64 KiB and 4,294,967,295 bytes; ceilings are rounded down to a whole number of logical sectors. Thus `4MB` and `4MiB` are different limits.

### Response times and bounded waits

The response-time row shows **mean / median**, to three decimal places, separately for OK and NG completions. Both statistics cover all completed reads since startup, including source reference checks; reads still pending are not counted as NG, and Ctrl+C cancellation is excluded. Delayed completion time is measured until completion is observed, so it may include time before the scanner next polls the result.

The single transfer-rate row shows **mean / median** separately for normal success (completed without being retained as pending) and delayed success. Mean speed is total successful bytes divided by summed request durations; median speed is the median of individual successful request rates. Units switch automatically between decimal MB/s, KB/s and B/s. Failed reads and reference checks are excluded from speed statistics. Concurrent request durations overlap, so these values are not wall-clock rescue throughput. All samples since startup are used, not a rolling window; statistics reset on restart. Medians are updated incrementally without copying or sorting the entire history for each display frame.

- `--timeout 2` is the initial/minimum time before looking elsewhere, not a cancellation deadline. Each sequential stream scales this time from its successful scan duration and request size, with separate normal and loaded profiles. New streams begin with the initial timeout. The original OVERLAPPED request retains its handle, event and buffer; late successes are saved without resuming that stream or adding fast-neighbor candidates.
- At scan or pending-buffer saturation, check the fastest of up to **30 recent distinct successful reference candidates**. Candidates are held only in memory and are not listed in the screen map or serialized into the map file. A scan's whole-request duration is a provisional observation until replaced by a successful one-sector check. The check timeout is that candidate's latest successful duration **plus two seconds**. With no observation, health/recovery checks use the greater of `--operation-timeout` and `--timeout`; initial resume verification uses `--operation-timeout`. A pending check is retained instead of duplicated, and repeated checks do not restart its time budget. Reference verification is also used when only retry ranges remain.
- `--max-parallel-reads` limits outstanding **scan** requests (default **16**, range **1–64**). Reference checks have one separate slot: at most 17 requests by default, or 65 at the largest scan limit. New requests are added one at a time as scans time out; slots are not filled immediately. At saturation, pause new scan requests, verify the reference and decide whether to attempt recovery. A successful reference check waits for scan capacity rather than adding further scans.
- Retained I/O buffers have a **2 GiB** aggregate limit; scans leave 64 KiB of that budget available for a reference check. Individual scan requests are capped below 2 GiB even if `--max-read-size` is larger. Reference checks read one logical sector.
- A failed or overdue reference check can trigger recovery. `--operation-timeout` also controls slow native-operation notices and reset-helper observation; it does not forcibly terminate pending reads or reset helpers. Only one reset helper may be outstanding. Health/reconnection checks continue, and scanning can resume once identity, geometry, reference bytes and volume protection are verified.
- A blocked Windows call may still delay its calling thread while the display remains animated. Pending requests do not imply permanently bad sectors. A failed coarse request identifies a range needing finer scans, not which individual sector caused it.

### Safe interruption, removal, and resume

**Ctrl+C and Ctrl+Break are supported graceful stop operations.** The application requests cancellation of the current read, keeps still-referenced Windows I/O resources alive until completion or process exit, flushes image data, saves the map, and exits. Image writes are split into 1 MiB chunks so a stop can checkpoint fully written sectors without waiting for an entire large request to be written. Wait for normal shutdown rather than closing the terminal or force-killing it; native calls may delay shutdown. An outstanding reset is retained until its completion notification arrives. Run the same command with the same source and output to resume safely; rescued sectors are skipped. No Enter key is required.

**Removing and reinserting the same SD card during imaging is supported.** A scan timeout proceeds to another region; it does not immediately enter reconnection. When recovery is required, new scans pause while reconnection is checked at intervals of at least one second. The reopened source must have the expected capacity and sector size, matching reference bytes, and protected volumes before scanning resumes. Reconnection does not require Enter. A transient disconnect does not mark unread bytes as recovered.

Before imaging begins, sd2img opens its read-only physical-disk handle, then locks and dismounts every enumerated source volume, including volumes without letters. No source data read is issued until volume protection succeeds. Geometry/length queries use a synchronous handle with `GENERIC_READ`, which is required by the length-query IOCTL even in an elevated process. Close applications using the card if locking fails. An initial failure to identify, lock, or dismount a relevant volume stops imaging and leaves the error available for review. During reconnection, volume-protection failures are logged and retried without issuing unprotected scan reads. The locks remain open while scanning; they are temporarily released for a device reset and reacquired before resuming. Mount-point assignments are not deleted, and a disk without exposed volumes can be read directly. Source handles and locks are released as the scan unwinds, before the final acknowledgement screen; Windows can then mount the volumes again when accessed.

Keep `card.img` and `card.img.map` together. A map with recovered sectors requires its matching full-size image; missing or differently sized images are rejected without recreating or resizing them. Map ranges must cover the expected source capacity exactly, without gaps or overlaps. A nonempty image without a matching map is not overwritten.

Device size and a reference sector are consistency checks, not unique device identification. Always reinsert the **same card**. Disk numbers can change after reinsertion; check the number if restarting with `-d`. These operations protect application state; they cannot guarantee that a failing card, reader, or driver will recover. Driver cancellation may itself be delayed, and the display distinguishes that wait. Forced termination, power loss, and hardware failure are not equivalent to a graceful Ctrl+C stop.

**Completion and errors remain visible until explicitly dismissed.** With terminal input, press Enter to exit after completion or an error; redirected/non-terminal input keeps the process alive until an explicit stop request. This acknowledgement is separate from recovery retries. Graceful Ctrl+C/Ctrl+Break interruption skips the Enter prompt after cleanup. CLI help/version and argument-parser errors are handled by the CLI parser and do not use this acknowledgement screen.

### Automatic device recovery

When reference verification fails at scan saturation (or when only pending sectors remain), sd2img checkpoints the image/map, closes its source handles, and tries recovery in this order:

1. **Configuration Manager API:** remove/restart/re-enumerate the selected disk device instance. It does not disable the device persistently or reset the USB controller or sibling disks.
2. **SetupAPI:** after the CM helper has completed with failure, or completed without a verifiable device response, enable/restart the same disk using `DIF_PROPERTYCHANGE`. A still-pending CM helper does not start a fallback helper.
3. If neither restores a readable, matching reference sector, suggest that reconnecting the same card **may help**, while continuing automatic retries.

Each attempt and result appears in the five-row notification area and the `.log` file. A successful API call alone is insufficient: capacity and reference bytes must also match. Reset attempts are not duplicated, and failed methods wait for their retry cooldown.

Potentially blocking PnP calls run in one hidden sd2img helper. The helper does not read source data: the scanner owns all reads and enforces the scan limit plus the separate reference slot. After the operation interval, an unfinished helper is retained and polled rather than killed; no fallback helper starts until it has finished. CM failure or failed reference verification permits SetupAPI fallback; a delayed CM failure also permits fallback after its notification arrives. API completion and verified reading are reported separately. Normal discovery and imaging do not launch PowerShell. If the exact instance cannot be identified, no other device is reset.

### Output and map format

The image is sized to the full source disk. In a newly created image, unread regions initially contain zeros; only map ranges marked `+` are known to have been read successfully. File size alone does not establish recovery completeness.

The companion map is a project-specific text format, **not a GNU ddrescue mapfile**:

- Sector-size line: `SECTOR_SIZE <logical sector bytes>`. If absent, sectors are interpreted as 512 bytes.
- Reference line: `GOOD <reference LBA> <one logical sector encoded as hexadecimal>`.
  Each successful range written to the image replaces the reference with its final sector, including delayed completions. This single persisted reference is separate from the runtime pool of up to 30 timed candidates. It is saved at checkpoints (when control returns to the scan loop after about three seconds, before recovery, and on shutdown); blocking I/O can delay a periodic checkpoint.
- Following lines: `<start LBA> <sector count> <status>`.

| Status | Meaning |
| --- | --- |
| `?` | Untried. |
| `+` | Successfully read and written to the image. |
| `~` | Coarse read failed or remained pending; awaiting single-sector scan. |
| `*` | Awaiting retry after a single-sector failure or an overlap with an outstanding request. |
| `-` | Marked bad; skipped unless `--retry-bad` is used. |

Rescued (`+`) ranges never revert to untried or failed during retries. Delayed reference-check completions verify the saved bytes and update candidate timing without rewriting rescued image data. Failed requests are recorded as `~` or `*` for finer scans or retries.

## Data handling and limitations

- The physical source handle is read-only. However, Windows flushes cached filesystem writes when a volume is locked, so preparation is not guaranteed to be write-free. Volume locks block ordinary filesystem users; they do not guarantee protection from other privileged software accessing the physical disk directly.
- Save output to a separate disk. sd2img checks the image, map, temporary map, and log destinations and rejects paths on the source disk before locking. A map with rescued sectors requires its matching full-size image and never silently recreates or resizes it. Use a new path for each source and retain the correct image/map pair.
- New sessions require a readable reference sector within the first 10,000 sectors. Resume also requires that saved reference sector to remain readable and match.
- Scheduling delays do not cancel reads. An unresponsive device or driver may retain outstanding slots indefinitely.
- Periodic map saves are not a transactional durability guarantee for image data during a crash or power loss. Prefer a graceful stop and keep backups of valuable recovery output.
- This tool produces raw images; it does not repair filesystems or extract individual files, and recovery is not guaranteed.
- Images contain source-disk data; maps contain a full reference sector; logs contain paths and recovery details. Treat these artifacts as sensitive and do not upload them to issues or pull requests.

## Development

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

Tests use synthetic data, temporary files, and named pipes rather than physical cards. Pending Windows reads without timeout cancellation, delayed successful completion, shared concurrency limits, Ctrl+C cancellation, a disappearing read endpoint, 512-byte/4-KiB map compatibility and reads, checkpoint/resume consistency, transfer-efficiency tuning, adaptive scheduling, reset fallback ordering, separate latency groups, append-only logging, localization, terminal resizing, and operation-monitor bookkeeping are covered. Unit tests live in `#[cfg(test)] mod tests` inside each source module; they are not compiled into the production executable. See [CONTRIBUTING.md](CONTRIBUTING.md) for contribution and reporting guidelines and [SECURITY.md](SECURITY.md) for vulnerability reporting.

### Publishing a GitHub Release

The [release workflow](.github/workflows/release.yml) runs when a stable `vMAJOR.MINOR.PATCH` tag (for example `v1.2.3`) is pushed. The tag's commit must be contained in `main`; the workflow checks this before building. Commit the workflow and its scripts to `main` before creating a release tag.

```powershell
git switch main
git pull --ff-only
git tag v1.2.3
git push origin main
git push origin v1.2.3
```

The tag defines the executable version. The workflow applies that version to `Cargo.toml` and the sd2img entry in `Cargo.lock` for the build, then runs release-tool tests, Rust formatting, Clippy and tests, builds Windows x64, and verifies `--version`. Version edits are local to the build; the pushed tag and source branches remain unchanged. The C runtime is statically linked so the executable can be distributed on its own; building does not create recovery images, maps or logs.

Assets are `sd2img-v1.2.3-windows-x86_64.exe` and its optional `.sha256` checksum file. Users need only the exe to run the tool. A draft release receives both assets before publication. `gh release create --generate-notes` uses GitHub's [automatically generated release notes](https://docs.github.com/en/repositories/releasing-projects-on-github/automatically-generated-release-notes), including merged PRs, contributors and a changelog link. Existing published releases are left intact on rerun; an incomplete draft can be retried with the same tag.

The workflow uses the repository's `GITHUB_TOKEN` with `contents: write`. Enable Actions and allow that permission under repository/organization policy and any tag rules. No extra personal access token is normally needed. Release runs are serialized, with up to 100 pending runs queued. The release helpers require Python 3.11 or newer; CI installs Python 3.14. To run their tests locally:

```powershell
python -m unittest discover -s .github/scripts -p "test_*.py" -v
```

### Source layout

`build.rs` embeds a build timestamp identifier in the executable for session-log identification. Windows CI runs formatting, Clippy, synthetic tests, and a locked release build.

| Module | Purpose |
| --- | --- |
| `main.rs` | Application startup, elevation, and display lifetime. |
| `cli.rs`, `i18n.rs` | Arguments and Windows UI language selection. |
| `device.rs`, `control.rs`, `geometry.rs`, `sector.rs` | Windows I/O, geometry, sector units, reconnection, and interruption. |
| `scanner.rs`, `scan_order.rs`, `read_size.rs`, `read_lanes.rs` | Rescue orchestration, fast-neighbor scheduling, per-stream tuning, and normal/loaded profiles. |
| `rescue_map.rs`, `image.rs`, `probes.rs` | Map format, runtime reference candidates, resume/reference validation, and checkpoints. |
| `visual.rs`, `output.rs`, `logging.rs` | Full-screen presentation, message formatting, and append-only logs. |
| `volumes.rs`, `windows_native.rs`, `reset.rs`, `watchdog.rs` | Volume protection, discovery/elevation, recovery, and bounded-operation supervision. |


## License

Licensed under the [MIT License](LICENSE).
