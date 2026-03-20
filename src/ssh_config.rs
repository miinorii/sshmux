use std::io::BufReader;

use ssh2_config::{ParseRule, SshConfig};

/// A single entry from `~/.ssh/config` that can be dialled directly.
#[derive(Clone)]
pub struct SshHost {
    pub label: String,
}

/// Strip a leading UTF-8 BOM if present (common on Windows).
fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{feff}').unwrap_or(s)
}

/// Parse SSH config content from a string.
fn parse_str(content: &str) -> Vec<SshHost> {
    let clean = strip_bom(content);
    let mut reader = BufReader::new(clean.as_bytes());
    let config = match SshConfig::default().parse(&mut reader, ParseRule::ALLOW_UNKNOWN_FIELDS) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    extract_hosts(&config)
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
    parse_str(&content)
}

/// Extract non-wildcard, non-negated host labels from a parsed config.
fn extract_hosts(config: &SshConfig) -> Vec<SshHost> {
    let mut hosts = Vec::new();
    for host in config.get_hosts() {
        for clause in &host.pattern {
            if clause.negated {
                continue;
            }
            let name = &clause.pattern;
            if name.is_empty() || name.contains('*') || name.contains('?') {
                continue;
            }
            hosts.push(SshHost {
                label: name.clone(),
            });
        }
    }
    hosts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(config: &str) -> Vec<String> {
        parse_str(config).into_iter().map(|h| h.label).collect()
    }

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

    #[test]
    fn empty_config() {
        assert!(parse("").is_empty());
    }

    #[test]
    fn host_with_multiple_patterns() {
        // "Host foo bar" defines two patterns for the same block
        let hosts = parse("Host foo bar\n    HostName example.com\n");
        assert_eq!(hosts, vec!["foo", "bar"]);
    }

    #[test]
    fn first_host_not_skipped() {
        // Regression: ensure the very first Host entry is included
        let content = "AddKeysToAgent yes\nIdentityFile ~/.ssh/id_ed25519\n\n\
                        Host first\n    HostName a.com\nHost second\n    HostName b.com\n";
        let hosts = parse(&content);
        assert_eq!(hosts, vec!["first", "second"]);
    }

    #[test]
    fn utf8_bom_first_host_not_lost() {
        let content = "\u{feff}Host first\n    HostName a.com\nHost second\n    HostName b.com\n";
        let hosts = parse(&content);
        assert_eq!(hosts, vec!["first", "second"]);
    }

    #[test]
    fn host_star_at_top_does_not_hide_entries() {
        let content = "Host *\n    ServerAliveInterval 60\n\n\
                        Host alpha\n    HostName a.com\nHost beta\n    HostName b.com\n";
        let hosts = parse(&content);
        assert_eq!(hosts, vec!["alpha", "beta"]);
    }

    #[test]
    fn negated_pattern_excluded() {
        // "Host !internal" — negated patterns should not appear as connectable hosts
        let hosts = parse("Host !internal\n");
        assert!(hosts.is_empty());
    }
}
