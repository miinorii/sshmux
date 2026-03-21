use std::{
    fs,
    path::{Path, PathBuf},
};

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
                    let mut found_st = false;
                    while i < raw.len() && raw[i] != 0x07 {
                        if raw[i] == 0x1b && i + 1 < raw.len() && raw[i + 1] == b'\\' {
                            i += 2;
                            found_st = true;
                            break;
                        }
                        i += 1;
                    }
                    if !found_st && i < raw.len() {
                        i += 1; // skip BEL
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
    if let Some(l) = lines.iter().find(|l| l.contains("working directory"))
        && let Some(path) = l.split_once(':').map(|x| x.1)
    {
        let p = path.trim();
        if !p.is_empty() {
            return Some(p.to_string());
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
            || line.starts_with("SSHMUX>")
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
        if name.is_empty() {
            continue;
        }

        let name = if is_link {
            name.split(" -> ").next().unwrap_or(&name).to_string()
        } else {
            name
        };

        // Strip shell quoting GNU ls adds in PTY mode (e.g. 'file name' or "file name")
        let name = strip_shell_quote(&name);

        // Strip directory prefix (from `ls -la /path` output)
        let name = name.rsplit('/').next().unwrap_or(&name).to_string();
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }

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
        // SCP uses \r to overwrite progress on the same line; take the last segment.
        let segment = l.rsplit('\r').next().unwrap_or(l);
        let segment = segment.trim();
        segment
            .split_whitespace()
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
        let leap = y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400));
        let ydays = if leap { 366 } else { 365 };
        if d < ydays {
            break;
        }
        d -= ydays;
        y += 1;
    }
    let leap = y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400));
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

/// Returns a list of available root paths to browse.
/// On Windows this is all accessible drive letters; on Unix it is just `/`.
pub fn list_drives() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        ('A'..='Z')
            .map(|c| PathBuf::from(format!("{}:\\", c)))
            .filter(|p| p.exists())
            .collect()
    }
    #[cfg(not(windows))]
    {
        vec![PathBuf::from("/")]
    }
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

/// Remove shell-style quoting that GNU `ls` adds in PTY mode.
/// Only strips if the inner content does NOT contain the same quote char,
/// which avoids false positives on filenames that legitimately contain quotes.
fn strip_shell_quote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        let inner = &s[1..s.len() - 1];
        if !inner.contains('\'') {
            return inner.to_string();
        }
    }
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        let inner = &s[1..s.len() - 1];
        if !inner.contains('"') {
            return inner.to_string();
        }
    }
    s.to_string()
}

// ---------------------------------------------------------------------------
// Re-export PathBuf for callers that need it via this module
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ls(raw: &str) -> Vec<String> {
        raw.lines().map(|l| l.to_string()).collect()
    }

    // ---- parse_pwd ---------------------------------------------------------

    #[test]
    fn parse_pwd_label() {
        assert_eq!(
            parse_pwd(&["Remote working directory: /home/debian".to_string()]),
            Some("/home/debian".to_string())
        );
    }

    #[test]
    fn parse_pwd_root() {
        assert_eq!(
            parse_pwd(&["Remote working directory: /".to_string()]),
            Some("/".to_string())
        );
    }

    #[test]
    fn parse_pwd_bare_absolute_path() {
        assert_eq!(
            parse_pwd(&["/home/user".to_string()]),
            Some("/home/user".to_string())
        );
    }

    #[test]
    fn parse_pwd_tilde_path() {
        assert_eq!(
            parse_pwd(&["~/projects".to_string()]),
            Some("~/projects".to_string())
        );
    }

    #[test]
    fn parse_pwd_path_with_spaces_ignored() {
        // paths with spaces are not valid bare pwd output
        assert_eq!(parse_pwd(&["/home/my dir".to_string()]), None);
    }

    #[test]
    fn parse_pwd_empty_input() {
        assert_eq!(parse_pwd(&[]), None);
    }

    #[test]
    fn parse_pwd_skips_noise_lines() {
        let lines = ls("sftp> pwd\nRemote working directory: /var/www\nsftp>");
        assert_eq!(parse_pwd(&lines), Some("/var/www".to_string()));
    }

    // ---- parse_ls ----------------------------------------------------------

    #[test]
    fn parse_ls_file_and_dir() {
        let e = parse_ls(&ls(
            "drwx------    ? debian  debian  4096 Mar 14 09:44 docs\n\
             -rw-r--r--    ? debian  debian   220 Aug  4  2021 .bashrc\n\
             sftp>",
        ));
        assert_eq!(e.len(), 3); // ".." + docs + .bashrc
        assert!(e.iter().any(|x| x.name == "docs"));
        assert!(e.iter().any(|x| x.name == ".bashrc"));
    }

    #[test]
    fn parse_ls_dirs_sorted_before_files() {
        let e = parse_ls(&ls("-rw-r--r--    ? u g   100 Jan  1  2020 aaa.txt\n\
             drwxr-xr-x    ? u g  4096 Jan  1  2020 zzz_dir"));
        assert_eq!(e[0].name, "..");
        assert_eq!(e[1].name, "zzz_dir");
        assert_eq!(e[2].name, "aaa.txt");
    }

    #[test]
    fn parse_ls_skips_dot_and_dotdot() {
        let e = parse_ls(&ls("drwx------    ? u g 4096 Mar 14 09:44 .\n\
             drwx------    ? u g 4096 Jan  1  2020 .."));
        // only the synthetic ".." entry remains
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].name, "..");
    }

    #[test]
    fn parse_ls_symlink_strips_arrow() {
        let e = parse_ls(&ls(
            "lrwxrwxrwx    ? u g 11 Jan  1  2020 mylink -> /etc/target",
        ));
        let link = e.iter().find(|x| x.name == "mylink");
        assert!(link.is_some());
        assert!(link.unwrap().is_dir); // symlinks treated as dirs for navigation
    }

    #[test]
    fn parse_ls_skips_noise_lines() {
        let e = parse_ls(&ls(
            "sftp>\ntotal 42\n-rw-r--r--    ? u g 100 Jan  1  2020 file.txt",
        ));
        assert_eq!(e.len(), 2);
    }

    #[test]
    fn parse_ls_masked_permissions() {
        let e = parse_ls(&ls("drwx******    ? u g 4096 Mar 14 09:44 somedir"));
        assert!(e.iter().find(|x| x.name == "somedir").unwrap().is_dir);
    }

    #[test]
    fn parse_ls_records_perms_and_modified() {
        let e = parse_ls(&ls("-rw-r--r--    ? u g 100 Jan  1 12:00 notes.txt"));
        let f = e.iter().find(|x| x.name == "notes.txt").unwrap();
        assert_eq!(f.perms, "-rw-r--r--");
        assert_eq!(f.modified, "Jan 1 12:00");
    }

    #[test]
    fn parse_ls_filename_with_spaces() {
        let e = parse_ls(&ls(
            "-rw-r--r--    ? u g 100 Jan  1  2020 my great file.txt",
        ));
        assert!(e.iter().any(|x| x.name == "my great file.txt"));
    }

    #[test]
    fn parse_ls_shell_quoted_filename() {
        // GNU ls wraps names with special chars in single quotes in PTY mode
        let quoted_name = format!("'{}'", "file [1] (copy).txt");
        let line = format!("-rw-r--r--    ? u g 100 Jan  1  2020 {}", quoted_name);
        let e = parse_ls(&ls(&line));
        assert!(e.iter().any(|x| x.name == "file [1] (copy).txt"));
    }

    #[test]
    fn parse_ls_filename_with_internal_quote() {
        // A filename like it's_a_file.txt must NOT be mangled
        let line = "-rw-r--r--    ? u g 100 Jan  1  2020 it's_a_file.txt";
        let e = parse_ls(&ls(line));
        assert!(e.iter().any(|x| x.name == "it's_a_file.txt"));
    }

    // ---- strip_ansi --------------------------------------------------------

    #[test]
    fn strip_ansi_plain_text_unchanged() {
        assert_eq!(strip_ansi(b"hello"), "hello");
    }

    #[test]
    fn strip_ansi_removes_csi_colour() {
        assert_eq!(strip_ansi(b"\x1b[32mhi\x1b[0m"), "hi");
    }

    #[test]
    fn strip_ansi_removes_osc_title() {
        assert_eq!(strip_ansi(b"\x1b]0;title\x07x"), "x");
    }

    #[test]
    fn strip_ansi_removes_bare_escape() {
        assert_eq!(strip_ansi(b"\x1bMtext"), "text");
    }

    #[test]
    fn strip_ansi_empty_input() {
        assert_eq!(strip_ansi(b""), "");
    }

    // ---- human_size --------------------------------------------------------

    #[test]
    fn hs_bytes() {
        assert_eq!(human_size(500), "500 B");
    }
    #[test]
    fn hs_kb() {
        assert_eq!(human_size(1024), "1.0 KB");
    }
    #[test]
    fn hs_mb() {
        assert_eq!(human_size(1024 * 1024), "1.0 MB");
    }
    #[test]
    fn hs_gb() {
        assert_eq!(human_size(1024 * 1024 * 1024), "1.0 GB");
    }
    #[test]
    fn hs_zero() {
        assert_eq!(human_size(0), "0 B");
    }

    // ---- shell_quote -------------------------------------------------------

    #[test]
    fn sq_plain() {
        assert_eq!(shell_quote("hello"), "'hello'");
    }
    #[test]
    fn sq_spaces() {
        assert_eq!(shell_quote("my file"), "'my file'");
    }
    #[test]
    fn sq_single_quote() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }
    #[test]
    fn sq_empty() {
        assert_eq!(shell_quote(""), "''");
    }

    // ---- skip_n_tokens -----------------------------------------------------

    #[test]
    fn snt_zero() {
        assert_eq!(skip_n_tokens("a b c", 0), "a b c");
    }
    #[test]
    fn snt_one() {
        assert_eq!(skip_n_tokens("a b c", 1), "b c");
    }
    #[test]
    fn snt_all() {
        assert_eq!(skip_n_tokens("a b c", 3), "");
    }
    #[test]
    fn snt_preserves_spaces_in_filename() {
        assert_eq!(
            skip_n_tokens("-rw-r--r-- 1 u g 100 Jan 1 12:00 my great file.txt", 8),
            "my great file.txt"
        );
    }

    // ---- epoch_to_ymd ------------------------------------------------------

    #[test]
    fn epoch_unix_origin() {
        assert_eq!(epoch_to_ymd(0), (1970, 1, 1, 0, 0));
    }

    #[test]
    fn epoch_known_date() {
        let (y, mo, d, _, _) = epoch_to_ymd(1710374400); // 2024-03-14
        assert_eq!((y, mo, d), (2024, 3, 14));
    }

    #[test]
    fn epoch_leap_year() {
        // 2000-02-29 00:00:00 UTC
        let (y, mo, d, _, _) = epoch_to_ymd(951782400);
        assert_eq!((y, mo, d), (2000, 2, 29));
    }

    #[test]
    fn epoch_day_after_leap_day() {
        // 2000-03-01 00:00:00 UTC
        let (y, mo, d, _, _) = epoch_to_ymd(951868800);
        assert_eq!((y, mo, d), (2000, 3, 1));
    }

    #[test]
    fn epoch_new_years_eve() {
        // 2023-12-31 23:59:00 UTC
        let (y, mo, d, h, mi) = epoch_to_ymd(1704067140);
        assert_eq!((y, mo, d, h, mi), (2023, 12, 31, 23, 59));
    }

    #[test]
    fn epoch_non_leap_century() {
        // 1900 is NOT a leap year (divisible by 100 but not 400)
        // 1970-03-01 00:00:00 UTC = 5097600
        let (y, mo, d, _, _) = epoch_to_ymd(5097600);
        assert_eq!((y, mo, d), (1970, 3, 1));
    }

    // ---- scrape_transfer_progress ------------------------------------------

    #[test]
    fn scrape_progress_simple() {
        assert_eq!(
            scrape_transfer_progress(&["50%".to_string()]),
            Some("50%".to_string())
        );
    }

    #[test]
    fn scrape_progress_zero() {
        assert_eq!(
            scrape_transfer_progress(&["0%".to_string()]),
            Some("0%".to_string())
        );
    }

    #[test]
    fn scrape_progress_hundred() {
        assert_eq!(
            scrape_transfer_progress(&["100%".to_string()]),
            Some("100%".to_string())
        );
    }

    #[test]
    fn scrape_progress_no_match() {
        assert_eq!(
            scrape_transfer_progress(&["no percent here".to_string()]),
            None
        );
    }

    #[test]
    fn scrape_progress_empty() {
        assert_eq!(scrape_transfer_progress(&[]), None);
    }

    #[test]
    fn scrape_progress_carriage_return() {
        // SCP overwrites progress with \r; last segment wins
        assert_eq!(
            scrape_transfer_progress(&["10%\r75%".to_string()]),
            Some("75%".to_string())
        );
    }

    #[test]
    fn scrape_progress_last_line_wins() {
        assert_eq!(
            scrape_transfer_progress(&["10%".to_string(), "90%".to_string()]),
            Some("90%".to_string())
        );
    }

    #[test]
    fn scrape_progress_with_surrounding_text() {
        assert_eq!(
            scrape_transfer_progress(&["demo.png  50% 125KB 62.5KB/s 00:02".to_string()]),
            Some("50%".to_string())
        );
    }

    #[test]
    fn scrape_progress_invalid_percent() {
        // "abc%" should not match (abc is not a u32)
        assert_eq!(scrape_transfer_progress(&["abc%".to_string()]), None);
    }

    #[test]
    fn scrape_progress_negative_percent() {
        // "-5%" should not match (negative not valid u32)
        assert_eq!(scrape_transfer_progress(&["-5%".to_string()]), None);
    }

    #[test]
    fn scrape_progress_scp_full_line() {
        // Realistic SCP output with \r-separated segments
        let line =
            "demo.png   0%    0     0.0KB/s   --:-- ETA\rdemo.png 100%  125KB  62.5KB/s   00:02";
        assert_eq!(
            scrape_transfer_progress(&[line.to_string()]),
            Some("100%".to_string())
        );
    }

    // ---- shell_quote (additional edge cases) -------------------------------

    #[test]
    fn sq_multiple_consecutive_quotes() {
        assert_eq!(shell_quote("a''b"), "'a'\\'''\\''b'");
    }

    #[test]
    fn sq_quote_at_end() {
        assert_eq!(shell_quote("end'"), "'end'\\'''");
    }

    #[test]
    fn sq_newline() {
        assert_eq!(shell_quote("hello\nworld"), "'hello\nworld'");
    }

    #[test]
    fn sq_backslash() {
        assert_eq!(shell_quote("back\\slash"), "'back\\slash'");
    }

    #[test]
    fn sq_special_chars() {
        assert_eq!(shell_quote("$HOME && rm -rf /"), "'$HOME && rm -rf /'");
    }

    // ---- parse_ls (additional edge cases) ----------------------------------

    #[test]
    fn parse_ls_unicode_filename() {
        let e = parse_ls(&ls("-rw-r--r--    ? u g 100 Jan  1  2020 café.txt"));
        assert!(e.iter().any(|x| x.name == "café.txt"));
    }

    #[test]
    fn parse_ls_sticky_bit() {
        let e = parse_ls(&ls("drwxrwxrwt    ? root root 4096 Jan  1  2020 tmp"));
        let dir = e.iter().find(|x| x.name == "tmp");
        assert!(dir.is_some());
        assert!(dir.unwrap().is_dir);
        assert_eq!(dir.unwrap().perms, "drwxrwxrwt");
    }

    #[test]
    fn parse_ls_setuid_bit() {
        let e = parse_ls(&ls("-rwsr-xr-x    ? root root 27104 Jan  1  2020 passwd"));
        let f = e.iter().find(|x| x.name == "passwd");
        assert!(f.is_some());
        assert_eq!(f.unwrap().perms, "-rwsr-xr-x");
    }

    #[test]
    fn parse_ls_block_device_skipped() {
        // Block devices start with 'b', not '-', 'd', or 'l' — should be skipped
        let e = parse_ls(&ls("brw-rw----    ? root disk 8 Jan  1  2020 0 sda"));
        // Only the synthetic ".." entry
        assert_eq!(e.len(), 1);
    }

    #[test]
    fn parse_ls_char_device_skipped() {
        let e = parse_ls(&ls("crw-rw-rw-    ? root tty 5 Jan  1  2020 0 tty"));
        assert_eq!(e.len(), 1);
    }

    #[test]
    fn parse_ls_zero_size() {
        let e = parse_ls(&ls("-rw-r--r--    ? u g 0 Jan  1  2020 empty.txt"));
        let f = e.iter().find(|x| x.name == "empty.txt").unwrap();
        assert_eq!(f.size, "0 B");
    }

    #[test]
    fn parse_ls_terabyte_file() {
        let e = parse_ls(&ls(
            "-rw-r--r--    ? u g 1099511627776 Jan  1  2020 large.bin",
        ));
        let f = e.iter().find(|x| x.name == "large.bin").unwrap();
        assert_eq!(f.size, "1.0 TB");
    }

    #[test]
    fn parse_ls_sshmux_prompt_skipped() {
        let e = parse_ls(&ls(
            "SSHMUX> ls -la /tmp\ntotal 12\n-rw-r--r--    ? u g 42 Jan  1  2020 data.csv",
        ));
        assert_eq!(e.len(), 2); // ".." + data.csv
    }

    #[test]
    fn parse_ls_empty_input() {
        let e = parse_ls(&[]);
        assert_eq!(e.len(), 1); // only synthetic ".."
        assert_eq!(e[0].name, "..");
    }

    #[test]
    fn parse_ls_only_noise() {
        let e = parse_ls(&ls("sftp>\ntotal 0\nls -la /tmp"));
        assert_eq!(e.len(), 1);
    }

    // ---- strip_ansi (additional edge cases) --------------------------------

    #[test]
    fn strip_ansi_esc_at_end_of_buffer() {
        // ESC at very end — should not panic
        assert_eq!(strip_ansi(b"text\x1b"), "text");
    }

    #[test]
    fn strip_ansi_partial_csi_at_end() {
        // Incomplete CSI sequence
        assert_eq!(strip_ansi(b"text\x1b[32"), "text");
    }

    #[test]
    fn strip_ansi_multiple_sequences() {
        assert_eq!(strip_ansi(b"\x1b[32m\x1b[1mhi\x1b[0m"), "hi");
    }

    #[test]
    fn strip_ansi_osc_with_st_terminator() {
        // OSC terminated by ESC \ instead of BEL
        assert_eq!(strip_ansi(b"\x1b]0;title\x1b\\text"), "text");
    }

    #[test]
    fn strip_ansi_mixed_content() {
        assert_eq!(
            strip_ansi(b"hello \x1b[31mred\x1b[0m world"),
            "hello red world"
        );
    }

    // ---- human_size (additional edge cases) --------------------------------

    #[test]
    fn hs_tb() {
        assert_eq!(human_size(1024u64 * 1024 * 1024 * 1024), "1.0 TB");
    }

    #[test]
    fn hs_just_under_kb() {
        assert_eq!(human_size(1023), "1023 B");
    }
}
