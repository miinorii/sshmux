use std::{fs, path::Path};

// ---------------------------------------------------------------------------
// ANSI stripping
// ---------------------------------------------------------------------------

/// Remove all ANSI/VT escape sequences from raw PTY bytes, returning plain text.
pub fn strip_ansi(raw: &[u8]) -> String {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == 0x1b {
            i += 1;
            if i >= raw.len() {
                break;
            }
            match raw[i] {
                b'[' => {
                    i += 1;
                    while i < raw.len() && !(0x40..=0x7e).contains(&raw[i]) {
                        i += 1;
                    }
                    i += 1;
                }
                b']' => {
                    i += 1;
                    while i < raw.len() && raw[i] != 0x07 {
                        if raw[i] == 0x1b && i + 1 < raw.len() && raw[i + 1] == b'\\' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                    if i < raw.len() {
                        i += 1;
                    }
                }
                _ => {
                    i += 1;
                }
            }
        } else {
            out.push(raw[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// SFTP screen-scraping helpers
// ---------------------------------------------------------------------------

/// Extract the remote working directory from `pwd` output.
pub fn parse_pwd(lines: &[String]) -> Option<String> {
    if let Some(l) = lines.iter().find(|l| l.contains("working directory")) {
        if let Some(path) = l.splitn(2, ':').nth(1) {
            let p = path.trim();
            if !p.is_empty() {
                return Some(p.to_string());
            }
        }
    }
    lines
        .iter()
        .find(|l| {
            let t = l.trim();
            (t.starts_with('/') || t.starts_with('~')) && !t.contains(' ')
        })
        .map(|l| l.trim().to_string())
}

/// A single entry in a directory listing (local or remote).
#[derive(Clone)]
pub struct FsEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: String,
    pub perms: String,
    pub modified: String,
}

/// Parse the output of `ls -la` into a `Vec<FsEntry>`.
pub fn parse_ls(lines: &[String]) -> Vec<FsEntry> {
    let mut entries = vec![FsEntry {
        name: "..".to_string(),
        is_dir: true,
        size: String::new(),
        perms: String::new(),
        modified: String::new(),
    }];
    for line in lines {
        let line = line.trim();
        if line.is_empty()
            || line.starts_with("sftp>")
            || line.starts_with("Remote")
            || line.starts_with("Changing")
            || line.starts_with("total")
            || line.starts_with("ls")
        {
            continue;
        }

        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 9 {
            continue;
        }

        let perms = tokens[0];
        let is_dir = perms.starts_with('d');
        let is_link = perms.starts_with('l');
        if !perms.starts_with('-') && !is_dir && !is_link {
            continue;
        }

        let size_bytes: u64 = tokens[4].parse().unwrap_or(0);
        let name = skip_n_tokens(line, 8).trim_end().to_string();
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }

        let name = if is_link {
            name.splitn(2, " -> ").next().unwrap_or(&name).to_string()
        } else {
            name
        };

        let modified = format!("{} {} {}", tokens[5], tokens[6], tokens[7]);
        entries.push(FsEntry {
            name,
            is_dir: is_dir || is_link,
            size: if is_dir {
                String::new()
            } else {
                human_size(size_bytes)
            },
            perms: perms.to_string(),
            modified,
        });
    }
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
    entries
}

/// Return the slice of `line` starting after skipping `n` whitespace-separated tokens.
pub fn skip_n_tokens(line: &str, n: usize) -> &str {
    let mut remaining = line.trim_start();
    for _ in 0..n {
        let end = remaining
            .find(|c: char| c.is_ascii_whitespace())
            .unwrap_or(remaining.len());
        remaining = &remaining[end..];
        remaining = remaining.trim_start();
    }
    remaining
}

/// Scrape a transfer progress percentage from sftp output lines.
pub fn scrape_transfer_progress(lines: &[String]) -> Option<String> {
    lines.iter().rev().find_map(|l| {
        let l = l.trim();
        l.split_whitespace()
            .find(|tok| tok.ends_with('%') && tok.trim_end_matches('%').parse::<u32>().is_ok())
            .map(|s| s.to_string())
    })
}

// ---------------------------------------------------------------------------
// Local filesystem helpers
// ---------------------------------------------------------------------------

/// Decompose a Unix timestamp into (year, month, day, hour, minute).
pub fn epoch_to_ymd(secs: u64) -> (u32, u32, u32, u32, u32) {
    let mi = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    let mut y = 1970u32;
    let mut d = days as u32;
    loop {
        let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
        let ydays = if leap { 366 } else { 365 };
        if d < ydays {
            break;
        }
        d -= ydays;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let month_days: [u32; 12] = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut mo = 0u32;
    for mlen in month_days {
        if d < mlen {
            break;
        }
        d -= mlen;
        mo += 1;
    }
    (y, mo + 1, d + 1, h as u32, mi as u32)
}

pub fn local_root() -> &'static str {
    if cfg!(windows) { "\\.\\" } else { "/" }
}

pub fn read_local_dir(path: &Path) -> Vec<FsEntry> {
    let mut entries = vec![FsEntry {
        name: "..".to_string(),
        is_dir: true,
        size: String::new(),
        perms: String::new(),
        modified: String::new(),
    }];
    if let Ok(rd) = fs::read_dir(path) {
        for entry in rd.flatten() {
            let meta = entry.metadata().ok();
            let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
            let size = meta
                .as_ref()
                .and_then(|m| {
                    if is_dir {
                        None
                    } else {
                        Some(human_size(m.len()))
                    }
                })
                .unwrap_or_default();
            let modified = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .map(|t| {
                    let secs = t
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let (y, mo, d, h, mi) = epoch_to_ymd(secs);
                    format!("{:04}-{:02}-{:02} {:02}:{:02}", y, mo, d, h, mi)
                })
                .unwrap_or_default();
            entries.push(FsEntry {
                name: entry.file_name().to_string_lossy().to_string(),
                is_dir,
                size,
                perms: String::new(),
                modified,
            });
        }
    }
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
    entries
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut val = bytes as f64;
    let mut unit = 0;
    while val >= 1024.0 && unit + 1 < UNITS.len() {
        val /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} B", bytes)
    } else {
        format!("{:.1} {}", val, UNITS[unit])
    }
}

pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ---------------------------------------------------------------------------
// Re-export PathBuf for callers that need it via this module
// ---------------------------------------------------------------------------
