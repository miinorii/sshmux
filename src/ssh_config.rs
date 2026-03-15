/// A single entry from `~/.ssh/config` that can be dialled directly.
#[derive(Clone)]
pub struct SshHost {
    pub label: String,
}

/// Parse SSH config content from a string and return all non-wildcard `Host` entries.
pub fn parse_ssh_config_from_str(content: &str) -> Vec<SshHost> {
    let mut hosts = Vec::new();
    for line in content.lines() {
        // Strip UTF-8 BOM that some Windows editors insert at the start of the file.
        let line = line.trim_start_matches('\u{feff}');
        let trimmed = line.trim();

        // Split on whitespace to separate key from value.
        let mut parts = trimmed.splitn(2, |c: char| c.is_whitespace());

        if let (Some(key), Some(value)) = (parts.next(), parts.next()) {
            if !key.eq_ignore_ascii_case("host") {
                continue;
            }

            // Strip inline comments, everything after the first '#' is ignored.
            // .split('#').next() never returns None so the unwrap is safe.
            let name = value.split('#').next().unwrap().trim();

            // Ignore empty values and catch-all glob patterns.
            if name.is_empty() || name.contains('*') || name.contains('?') {
                continue;
            }

            hosts.push(SshHost {
                label: name.to_string(),
            });
        }
    }
    hosts
}

/// Parse `~/.ssh/config` and return all non-wildcard `Host` entries.
pub fn parse_ssh_config() -> Vec<SshHost> {
    let config_path = match dirs::home_dir() {
        Some(h) => h.join(".ssh").join("config"),
        None => return vec![],
    };
    let content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    parse_ssh_config_from_str(&content)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(config: &str) -> Vec<String> {
        parse_ssh_config_from_str(config)
            .into_iter()
            .map(|h| h.label)
            .collect()
    }

    // ---- basic parsing -----------------------------------------------------

    #[test]
    fn simple_host_entry() {
        let hosts = parse("Host vps\n    HostName example.com\n");
        assert_eq!(hosts, vec!["vps"]);
    }

    #[test]
    fn multiple_hosts() {
        let hosts = parse("Host web\nHost db\nHost cache\n");
        assert_eq!(hosts, vec!["web", "db", "cache"]);
    }

    #[test]
    fn case_insensitive_keyword() {
        assert_eq!(parse("HOST myserver\n"), vec!["myserver"]);
        assert_eq!(parse("host myserver\n"), vec!["myserver"]);
        assert_eq!(parse("HoSt myserver\n"), vec!["myserver"]);
    }

    // ---- wildcards are excluded --------------------------------------------

    #[test]
    fn wildcard_star_excluded() {
        assert!(parse("Host *\n").is_empty());
    }

    #[test]
    fn wildcard_question_mark_excluded() {
        assert!(parse("Host host?\n").is_empty());
    }

    #[test]
    fn partial_wildcard_excluded() {
        assert!(parse("Host *.example.com\n").is_empty());
    }

    // ---- comments ----------------------------------------------------------

    #[test]
    fn inline_comment_stripped() {
        let hosts = parse("Host vps # my production server\n");
        assert_eq!(hosts, vec!["vps"]);
    }

    #[test]
    fn comment_only_line_ignored() {
        let hosts = parse("# this is a comment\nHost vps\n");
        assert_eq!(hosts, vec!["vps"]);
    }

    #[test]
    fn host_value_with_hash_in_comment() {
        let hosts = parse("Host prod # host=prod.example.com\n");
        assert_eq!(hosts, vec!["prod"]);
    }

    // ---- empty / degenerate input ------------------------------------------

    #[test]
    fn empty_config() {
        assert!(parse("").is_empty());
    }

    #[test]
    fn host_keyword_with_no_value() {
        assert!(parse("Host\n").is_empty());
    }

    #[test]
    fn host_keyword_with_only_whitespace() {
        assert!(parse("Host   \n").is_empty());
    }

    #[test]
    fn blank_lines_ignored() {
        let hosts = parse("\n\n\nHost vps\n\n\n");
        assert_eq!(hosts, vec!["vps"]);
    }

    // ---- BOM ---------------------------------------------------------------

    #[test]
    fn utf8_bom_stripped() {
        let hosts = parse("\u{feff}Host vps\n");
        assert_eq!(hosts, vec!["vps"]);
    }

    // ---- non-Host keys ignored ---------------------------------------------

    #[test]
    fn non_host_keys_ignored() {
        let hosts = parse("HostName example.com\nUser debian\nPort 22\nHost vps\n");
        assert_eq!(hosts, vec!["vps"]);
    }

    #[test]
    fn hostname_key_not_confused_with_host() {
        assert!(parse("HostName example.com\n").is_empty());
    }

    // ---- whitespace variants -----------------------------------------------

    #[test]
    fn tab_separated_key_value() {
        let hosts = parse("Host\tvps\n");
        assert_eq!(hosts, vec!["vps"]);
    }

    #[test]
    fn extra_whitespace_around_value() {
        let hosts = parse("Host   vps   \n");
        assert_eq!(hosts, vec!["vps"]);
    }
}
