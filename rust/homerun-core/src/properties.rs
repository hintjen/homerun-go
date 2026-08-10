//! Merging `key=value` config files without trampling what is already there.
//!
//! Reference: `mergeServerProperties:3981` in `nativeServerManager.ts`.
//!
//! # Why this is not in the Minecraft module
//!
//! `key=value` with `#` comments is the format Minecraft uses for
//! `server.properties`, and also what a great many other dedicated servers use
//! for theirs. The *keys* are a game's business; merging is not. Keeping the
//! merge here means a second game gets it for free and cannot reimplement it
//! slightly differently.
//!
//! # The contract
//!
//! Anything the host does not manage survives untouched and in place —
//! comments, blank lines, keys the server wrote itself, settings a player
//! edited by hand. Only the managed keys are overwritten, and they are
//! overwritten *where they sit*, so the file does not churn between launches.

/// Merge managed keys into an existing file.
///
/// `existing` may be empty, which produces a file of only the managed keys —
/// what happens on a world's first launch. Managed keys not already present
/// are appended in the order given, which is why the caller passes an ordered
/// list rather than a map.
///
/// # Encoding
///
/// This works in `str`, so the caller decides the encoding it reads and writes
/// with. That decision is not cosmetic: Minecraft's `server.properties` is
/// latin-1, and writing it as UTF-8 turns `§` — the colour-code marker in a
/// MOTD — into mojibake in every server list. [`crate::game::Encoding`]
/// carries the answer alongside the contents so a host never has to know.
pub fn merge(existing: &str, managed: &[(String, String)]) -> String {
    let lookup = |key: &str| managed.iter().find(|(k, _)| k == key);

    let mut written: Vec<&str> = Vec::new();
    let mut output: Vec<String> = Vec::new();

    // `"".split('\n')` yields one empty element, which would open the file
    // with a blank line. No file and an empty file are the same thing here.
    let lines: Vec<&str> = if existing.is_empty() {
        Vec::new()
    } else {
        existing.split('\n').collect()
    };

    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            output.push(line.to_string());
            continue;
        }
        let Some(eq) = trimmed.find('=') else {
            output.push(line.to_string());
            continue;
        };
        let key = trimmed[..eq].trim();
        match lookup(key) {
            Some((managed_key, value)) => {
                output.push(format!("{managed_key}={value}"));
                written.push(managed_key.as_str());
            }
            None => output.push(line.to_string()),
        }
    }

    for (key, value) in managed {
        if !written.contains(&key.as_str()) {
            output.push(format!("{key}={value}"));
        }
    }

    while output.last().is_some_and(|l| l.trim().is_empty()) {
        output.pop();
    }

    let mut text = output.join("\n");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn managed(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn an_absent_file_becomes_the_managed_keys() {
        let file = merge("", &managed(&[("a", "1"), ("b", "2")]));
        assert_eq!(file, "a=1\nb=2\n");
    }

    #[test]
    fn unmanaged_keys_and_comments_survive() {
        let existing = "#Minecraft server properties\n#Sun Aug 10 2026\nrate-limit=0\nmotd=old\n";
        let file = merge(existing, &managed(&[("motd", "new")]));
        assert!(file.contains("#Minecraft server properties"));
        assert!(
            file.contains("rate-limit=0"),
            "an unmanaged key was dropped"
        );
        assert!(file.contains("motd=new"));
        assert!(!file.contains("motd=old"));
    }

    /// Rewritten where it sits, so the file does not reorder between launches.
    #[test]
    fn a_managed_key_is_rewritten_in_place() {
        let file = merge("first=1\nmotd=old\nlast=2\n", &managed(&[("motd", "new")]));
        assert_eq!(file, "first=1\nmotd=new\nlast=2\n");
    }

    /// Two merges in a row must produce the same bytes, or every launch
    /// rewrites the file and any watcher sees a spurious change.
    #[test]
    fn merging_is_idempotent() {
        let keys = managed(&[("a", "1"), ("b", "2")]);
        let once = merge("existing=yes\n", &keys);
        assert_eq!(once, merge(&once, &keys));
    }

    #[test]
    fn a_line_without_an_equals_is_left_alone() {
        let file = merge("garbage line\n", &managed(&[("a", "1")]));
        assert!(file.contains("garbage line"));
        assert!(file.contains("a=1"));
    }

    /// Keys written with padding are still recognised as the same key, and
    /// the rewrite normalises them.
    #[test]
    fn whitespace_around_a_key_is_tolerated() {
        let file = merge("  motd = old\n", &managed(&[("motd", "new")]));
        assert_eq!(file, "motd=new\n");
    }

    /// A file read on Windows carries `\r`; the parse must still see the key,
    /// and the rewritten line must not keep a stray carriage return.
    #[test]
    fn crlf_input_is_handled() {
        let file = merge("motd=old\r\npvp=true\r\n", &managed(&[("motd", "new")]));
        assert!(file.contains("motd=new"), "{file}");
        assert!(
            !file.contains("motd=new\r"),
            "a stray CR was kept: {file:?}"
        );
        assert!(file.contains("pvp=true"), "unmanaged line lost: {file}");
    }

    #[test]
    fn a_value_containing_an_equals_survives() {
        let file = merge("", &managed(&[("level-seed", "a=b")]));
        assert_eq!(file, "level-seed=a=b\n");
        // And re-reading it does not truncate at the second `=`.
        assert_eq!(merge(&file, &managed(&[("level-seed", "a=b")])), file);
    }

    #[test]
    fn an_empty_managed_set_leaves_the_file_alone() {
        assert_eq!(merge("a=1\nb=2\n", &[]), "a=1\nb=2\n");
    }
}
