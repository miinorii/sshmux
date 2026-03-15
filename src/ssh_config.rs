/// A single entry from `~/.ssh/config` that can be dialled directly.
#[derive(Clone)]
pub struct SshHost {
    pub label: String,
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
    let mut hosts = Vec::new();
    for line in content.lines() {
        let line = line.trim_start_matches('\u{feff}');
        let trimmed = line.trim();
        let mut parts = trimmed.splitn(2, |c: char| c.is_whitespace());
        if let (Some(kw), Some(rest)) = (parts.next(), parts.next()) {
            if kw.eq_ignore_ascii_case("host") {
                let name = rest.trim();
                if !name.is_empty() && !name.contains('*') && !name.contains('?') {
                    hosts.push(SshHost {
                        label: name.to_string(),
                    });
                }
            }
        }
    }
    hosts
}
