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
        // remove BOM
        let line = line.trim_start_matches('\u{feff}');

        // Remove line endings and trailing whitespaces
        let trimmed = line.trim();

        // split on withespace
        let mut parts = trimmed.splitn(2, |c: char| c.is_whitespace());

        // extract the value of the 'Host' key
        if let (Some(key), Some(value)) = (parts.next(), parts.next()) {
            if !key.eq_ignore_ascii_case("host") {
                continue;
            }
            
            // remove comments, unwrap can't fail since .split always return something
            let name = value.split("#")
                .next()
                .unwrap()
                .trim();

            // ignore empty 'Host' and catch-all patterns
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
