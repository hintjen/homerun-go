//! Merging managed keys into a game's INI configuration file.
//!
//! The INI counterpart of [`crate::properties`] and [`crate::json_config`],
//! with the same contract: anything the host does not manage survives —
//! other sections, other keys, comments, blank lines, order — only the managed
//! keys change, and an unset setting takes its key *out* of the file rather
//! than leaving the previous launch's value behind.
//!
//! Unreal Engine's servers are why this exists, so it reads INI the way
//! Unreal writes it, and one Unreal habit gets first-class support: a whole
//! group of settings kept as one struct literal on one key. Palworld's
//! `PalWorldSettings.ini` is two lines:
//!
//! ```text
//! [/Script/Pal.PalGameWorldSettings]
//! OptionSettings=(ServerName="Homerun probe",AdminPassword="x",RCONEnabled=False,RESTAPIPort=8212)
//! ```
//!
//! # Keys
//!
//! A managed key names its section in brackets, because Unreal's section
//! names are paths full of `/` and `.` that no other delimiter survives:
//!
//! - `[HTTPServer.Listeners]DefaultBindAddress` is the `DefaultBindAddress=`
//!   line in that section.
//! - `[/Script/Pal.PalGameWorldSettings]OptionSettings(ServerName)` is the
//!   `ServerName` member of the struct on the `OptionSettings=` line.
//!
//! The member is in parentheses rather than after a dot because Unreal's own
//! keys have dots in them — `[SystemSettings]` is console variables like
//! `r.Shadow.MaxResolution` — and a dot that sometimes meant "member" would
//! make those unreachable. Names match the way Unreal matches them, ignoring
//! ASCII case; what the host writes is spelled as the descriptor spells it.
//!
//! A missing section is appended, a missing key is added to the end of its
//! section, and a missing struct is created holding only the managed members.
//! A sparse struct is valid: the server fills in the rest from its defaults,
//! and rewrites the file itself on shutdown.
//!
//! Lines Unreal treats as array operations (`+Key=`, `-Key=`, `.Key=`, `!Key`)
//! are never managed keys and are left alone.
//!
//! # Values keep their type
//!
//! As in [`crate::json_config`], the caller passes `serde_json::Value`s. A
//! boolean is `True`/`False`, the spelling Unreal writes; a number is bare.
//! Text is written raw on a plain line and double-quoted inside a struct,
//! where an unquoted string ends at the first comma.
//!
//! # What is refused rather than written
//!
//! A value is one line, so a control character — a newline above all, which
//! would start a key of its own — is refused wherever it would land. Inside a
//! struct a `"` ends the string early and whatever follows becomes members of
//! the struct: a server name could switch its own admin console on. Unreal has
//! a backslash escape, but no game has been seen to honour it, and a trailing
//! `\` escapes the closing quote instead; so text in a struct may contain
//! neither. The error names the key and never the value, which may be a
//! password.
//!
//! A file this cannot read with certainty is refused rather than guessed at:
//! a managed key that appears twice in its section, or a struct whose quotes
//! or parentheses do not balance. Rewriting either would be a guess about
//! which half the game reads.
//!
//! # Encoding and line endings
//!
//! Unreal saves an INI as UTF-16 when it holds anything outside ASCII, so a
//! server name with an accent in it comes back from the server's first
//! shutdown in a different encoding from the one it went in. [`decode`] reads
//! UTF-8 (with or without a byte order mark) and UTF-16LE, and [`encode`]
//! writes the file back the way it was found. Lines keep the file's own
//! endings; a new line uses CRLF if the file's first line does.

use serde_json::Value;
use std::fmt;

/// Why an INI configuration file could not be merged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A managed key appears on more than one line of its section.
    Duplicate(String),
    /// A managed struct member's key holds something other than a struct this
    /// can read: not parenthesised, or quotes or parentheses left open.
    NotAStruct(String),
    /// A managed value this format cannot hold. The value is never included:
    /// it may be a password.
    Unwritable { key: String, why: &'static str },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Duplicate(key) => write!(
                f,
                "the game configuration sets \"{key}\" more than once, so which one the game reads is unclear"
            ),
            Error::NotAStruct(key) => write!(
                f,
                "the game configuration has a value at \"{key}\" that is not a group of settings this can read"
            ),
            Error::Unwritable { key, why } => write!(f, "the value for \"{key}\" {why}"),
        }
    }
}

impl std::error::Error for Error {}

/// A managed key, parsed. See the module header for the syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key<'a> {
    pub section: &'a str,
    pub name: &'a str,
    /// The struct member, for `[Section]Key(Member)`.
    pub member: Option<&'a str>,
}

/// Parse a managed key, or `None` if it is not one.
///
/// [`crate::engine::validate`] refuses an unusable key in a descriptor before
/// a launch ever gets here.
pub fn parse_key(key: &str) -> Option<Key<'_>> {
    let rest = key.strip_prefix('[')?;
    let (section, rest) = rest.split_once(']')?;
    if section.is_empty()
        || section.trim() != section
        || section.contains(['[', ']'])
        || section.chars().any(char::is_control)
    {
        return None;
    }
    let (name, member) = match rest.split_once('(') {
        Some((name, member)) => (name, Some(member.strip_suffix(')')?)),
        None => (rest, None),
    };
    let usable_name = !name.is_empty()
        && !name.starts_with([';', '#', '+', '-', '.', '!'])
        && !name
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "=[]()\",".contains(c));
    if !usable_name {
        return None;
    }
    if let Some(member) = member {
        if member.is_empty()
            || !member
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return None;
        }
    }
    Some(Key {
        section,
        name,
        member,
    })
}

/// How a file was stored, so it can be written back the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Encoding {
    #[default]
    Utf8,
    Utf8Bom,
    Utf16Le,
}

/// Read a file's bytes as text, or `None` if they are not text this reads.
pub fn decode(bytes: &[u8]) -> Option<(String, Encoding)> {
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        if rest.len() % 2 != 0 {
            return None;
        }
        let units: Vec<u16> = (0..rest.len())
            .step_by(2)
            .map(|i| u16::from_le_bytes([rest[i], rest[i + 1]]))
            .collect();
        return String::from_utf16(&units)
            .ok()
            .map(|text| (text, Encoding::Utf16Le));
    }
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8(rest.to_vec())
            .ok()
            .map(|text| (text, Encoding::Utf8Bom));
    }
    String::from_utf8(bytes.to_vec())
        .ok()
        .map(|text| (text, Encoding::Utf8))
}

/// Write text back in the encoding [`decode`] found it in.
pub fn encode(text: &str, encoding: Encoding) -> Vec<u8> {
    match encoding {
        Encoding::Utf8 => text.as_bytes().to_vec(),
        Encoding::Utf8Bom => [&[0xEF, 0xBB, 0xBF][..], text.as_bytes()].concat(),
        Encoding::Utf16Le => [0xFF, 0xFE]
            .into_iter()
            .chain(text.encode_utf16().flat_map(u16::to_le_bytes))
            .collect(),
    }
}

/// Merge managed values into an existing file.
///
/// `existing` may be empty, which produces a file of only the managed keys.
/// `clear` names keys whose setting is unset: a plain key's line is removed,
/// and a struct member is taken out of its struct, which stays (as `()` if it
/// was the last) because the game may expect the key.
pub fn merge(existing: &str, set: &[(String, Value)], clear: &[String]) -> Result<String, Error> {
    let mut file = File::parse(existing);
    for key in clear {
        // An unusable key is a descriptor defect validate reports; skipping
        // it here keeps a stale descriptor from corrupting the file.
        let Some(parsed) = parse_key(key) else {
            continue;
        };
        file.clear(key, &parsed)?;
    }
    for (key, value) in set {
        let Some(parsed) = parse_key(key) else {
            continue;
        };
        let text = render(key, &parsed, value)?;
        file.set(key, &parsed, &text)?;
    }
    Ok(file.text())
}

/// A value as it is spelled in the file.
fn render(key: &str, parsed: &Key, value: &Value) -> Result<String, Error> {
    let refuse = |why| Error::Unwritable {
        key: key.to_string(),
        why,
    };
    match value {
        Value::Bool(true) => Ok("True".into()),
        Value::Bool(false) => Ok("False".into()),
        Value::Number(n) => Ok(n.to_string()),
        Value::String(text) => {
            if text.chars().any(char::is_control) {
                return Err(refuse(
                    "contains a line break or control character, which this configuration file cannot store",
                ));
            }
            if parsed.member.is_none() {
                return Ok(text.clone());
            }
            if text.contains(['"', '\\']) {
                return Err(refuse(
                    "contains a quotation mark or backslash, which this game's configuration cannot store safely",
                ));
            }
            Ok(format!("\"{text}\""))
        }
        _ => Err(refuse("is not text, a number or true/false")),
    }
}

/// A file as lines, each kept byte for byte until something replaces it.
struct File {
    lines: Vec<String>,
    eol: &'static str,
}

/// Where a section's lines are: its header, and one past its last line.
struct Span {
    header: usize,
    end: usize,
}

impl File {
    fn parse(text: &str) -> File {
        let eol = match text.split_once('\n') {
            Some((first, _)) if first.ends_with('\r') => "\r\n",
            _ => "\n",
        };
        // A final line ending does not open an empty last line.
        let body = text.strip_suffix('\n').unwrap_or(text);
        let lines = if text.is_empty() {
            Vec::new()
        } else {
            body.split('\n')
                .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
                .collect()
        };
        File { lines, eol }
    }

    fn text(&self) -> String {
        let mut text = self.lines.join(self.eol);
        if !self.lines.is_empty() {
            text.push_str(self.eol);
        }
        text
    }

    /// Every section with this name, in file order. Unreal reads repeated
    /// headers as one section.
    fn sections(&self, name: &str) -> Vec<Span> {
        self.lines
            .iter()
            .enumerate()
            .filter(|(_, line)| header(line).is_some_and(|h| h.eq_ignore_ascii_case(name)))
            .map(|(i, _)| Span {
                header: i,
                end: self.next_header(i + 1),
            })
            .collect()
    }

    fn next_header(&self, from: usize) -> usize {
        (from..self.lines.len())
            .find(|&i| header(&self.lines[i]).is_some())
            .unwrap_or(self.lines.len())
    }

    /// The one line setting `name` in `section`, if there is one.
    fn find(&self, key: &str, parsed: &Key) -> Result<Option<usize>, Error> {
        let mut found = None;
        for span in self.sections(parsed.section) {
            for i in span.header + 1..span.end {
                if entry(&self.lines[i]).is_some_and(|(k, _)| k.eq_ignore_ascii_case(parsed.name)) {
                    if found.is_some() {
                        return Err(Error::Duplicate(key.to_string()));
                    }
                    found = Some(i);
                }
            }
        }
        Ok(found)
    }

    fn clear(&mut self, key: &str, parsed: &Key) -> Result<(), Error> {
        let Some(i) = self.find(key, parsed)? else {
            return Ok(());
        };
        let Some(member) = parsed.member else {
            self.lines.remove(i);
            return Ok(());
        };
        let (prefix, value) = split_entry(&self.lines[i]);
        let mut members = members(value).ok_or_else(|| Error::NotAStruct(key.to_string()))?;
        let before = members.len();
        members.retain(|m| !member_name(m).eq_ignore_ascii_case(member));
        if members.len() != before {
            self.lines[i] = format!("{prefix}({})", members.join(","));
        }
        Ok(())
    }

    fn set(&mut self, key: &str, parsed: &Key, text: &str) -> Result<(), Error> {
        match (self.find(key, parsed)?, parsed.member) {
            (Some(i), None) => {
                let (prefix, _) = split_entry(&self.lines[i]);
                self.lines[i] = format!("{prefix}{text}");
            }
            (Some(i), Some(member)) => {
                let (prefix, value) = split_entry(&self.lines[i]);
                let mut members =
                    members(value).ok_or_else(|| Error::NotAStruct(key.to_string()))?;
                let fresh = format!("{member}={text}");
                match members
                    .iter()
                    .position(|m| member_name(m).eq_ignore_ascii_case(member))
                {
                    Some(at) => members[at] = fresh,
                    None => members.push(fresh),
                }
                self.lines[i] = format!("{prefix}({})", members.join(","));
            }
            (None, member) => {
                let line = match member {
                    Some(member) => format!("{}=({member}={text})", parsed.name),
                    None => format!("{}={text}", parsed.name),
                };
                self.add(parsed.section, line);
            }
        }
        Ok(())
    }

    /// Add a line to the end of a section, creating the section if needed.
    fn add(&mut self, section: &str, line: String) {
        if let Some(span) = self.sections(section).first() {
            // After the section's last line with anything on it, so the blank
            // line separating it from the next section stays where it is.
            let mut at = span.end;
            while at > span.header + 1 && self.lines[at - 1].trim().is_empty() {
                at -= 1;
            }
            self.lines.insert(at, line);
            return;
        }
        while self.lines.last().is_some_and(|l| l.trim().is_empty()) {
            self.lines.pop();
        }
        if !self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.lines.push(format!("[{section}]"));
        self.lines.push(line);
    }
}

/// A section header's name, or `None` if the line is not one.
fn header(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    trimmed.strip_prefix('[')?.strip_suffix(']').map(str::trim)
}

/// A `Key=Value` line's key and value, or `None` for a header, a comment, or
/// a line with no `=`.
fn entry(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim_start();
    if trimmed.starts_with([';', '#', '[']) {
        return None;
    }
    let (key, value) = trimmed.split_once('=')?;
    Some((key.trim(), value))
}

/// A key line split after its `=`, so a rewrite keeps the key as the file
/// spells it.
fn split_entry(line: &str) -> (&str, &str) {
    let eq = line
        .find('=')
        .expect("only called on a line entry() accepted");
    (&line[..=eq], &line[eq + 1..])
}

/// The members of a struct literal, each as written, or `None` if the value
/// is not one this can read with certainty.
///
/// Commas split members only outside quotes and nested parentheses:
/// `CrossplayPlatforms=(Steam,Xbox)` and `DenyTechnologyList=("A","B")` are
/// one member each.
fn members(value: &str) -> Option<Vec<String>> {
    let inner = value.trim().strip_prefix('(')?;
    let mut members = Vec::new();
    let mut current = String::new();
    let mut depth = 1usize;
    let mut quoted = false;
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if quoted {
            current.push(c);
            match c {
                '\\' => current.push(chars.next()?),
                '"' => quoted = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => quoted = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    // The struct must end the value.
                    if !chars.as_str().trim().is_empty() {
                        return None;
                    }
                    if !current.trim().is_empty() || !members.is_empty() {
                        members.push(current);
                    }
                    return members.iter().all(|m| m.contains('=')).then_some(members);
                }
            }
            ',' if depth == 1 => {
                members.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    None
}

fn member_name(member: &str) -> &str {
    member
        .split_once('=')
        .map_or(member, |(name, _)| name)
        .trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PAL: &str = "[/Script/Pal.PalGameWorldSettings]\nOptionSettings=(ServerName=\"Homerun probe\",AdminPassword=\"x\",RCONEnabled=False,RESTAPIEnabled=True,RESTAPIPort=8212)\n";
    const OPTION: &str = "[/Script/Pal.PalGameWorldSettings]OptionSettings";

    fn set(pairs: &[(&str, Value)]) -> Vec<(String, Value)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn member(name: &str) -> String {
        format!("{OPTION}({name})")
    }

    fn pal_values() -> Vec<(String, Value)> {
        vec![
            (member("ServerName"), json!("Homerun probe")),
            (member("AdminPassword"), json!("x")),
            (member("RCONEnabled"), json!(false)),
            (member("RESTAPIEnabled"), json!(true)),
            (member("RESTAPIPort"), json!(8212)),
        ]
    }

    #[test]
    fn keys_name_a_section_a_key_and_optionally_a_member() {
        assert_eq!(
            parse_key("[HTTPServer.Listeners]DefaultBindAddress"),
            Some(Key {
                section: "HTTPServer.Listeners",
                name: "DefaultBindAddress",
                member: None
            })
        );
        assert_eq!(
            parse_key(&member("ServerName")),
            Some(Key {
                section: "/Script/Pal.PalGameWorldSettings",
                name: "OptionSettings",
                member: Some("ServerName")
            })
        );
        // A dotted key is a key, not a member: console variables look like this.
        assert_eq!(
            parse_key("[SystemSettings]r.Shadow.MaxResolution").map(|k| k.name),
            Some("r.Shadow.MaxResolution")
        );
        for bad in [
            "Key",
            "[]Key",
            "[S]",
            "[S]Key()",
            "[S]Key(A.B)",
            "[S]Key(A",
            "[S]Ke y",
            "[S]Key=1",
            "[S]+Key",
            "[S]Key(A)B",
            "[ S]Key",
        ] {
            assert!(parse_key(bad).is_none(), "{bad} should not parse");
        }
    }

    /// The file observed on a real Palworld server, written from nothing and
    /// then merged again with the same values: identical bytes both times.
    #[test]
    fn the_palworld_settings_line_is_written_and_round_trips() {
        let out = merge("", &pal_values(), &[]).unwrap();
        assert_eq!(out, PAL);
        assert_eq!(merge(&out, &pal_values(), &[]).unwrap(), PAL);
    }

    /// The server rewrote the file on shutdown with a blank line after it,
    /// and on Windows it may come back CRLF.
    #[test]
    fn the_servers_own_rewrite_merges_without_churn() {
        let rewritten = format!("{PAL}\n");
        assert_eq!(merge(&rewritten, &pal_values(), &[]).unwrap(), rewritten);
        let crlf = rewritten.replace('\n', "\r\n");
        assert_eq!(merge(&crlf, &pal_values(), &[]).unwrap(), crlf);
    }

    #[test]
    fn a_struct_member_is_replaced_in_place_and_a_new_one_appended() {
        let out = merge(
            PAL,
            &set(&[
                (&member("RCONEnabled"), json!(true)),
                (&member("PublicPort"), json!(8211)),
            ]),
            &[],
        )
        .unwrap();
        assert_eq!(
            out,
            "[/Script/Pal.PalGameWorldSettings]\nOptionSettings=(ServerName=\"Homerun probe\",AdminPassword=\"x\",RCONEnabled=True,RESTAPIEnabled=True,RESTAPIPort=8212,PublicPort=8211)\n"
        );
    }

    #[test]
    fn an_unset_member_leaves_the_struct_and_nothing_else_does() {
        let out = merge(PAL, &[], &[member("AdminPassword"), member("Absent")]).unwrap();
        assert_eq!(
            out,
            "[/Script/Pal.PalGameWorldSettings]\nOptionSettings=(ServerName=\"Homerun probe\",RCONEnabled=False,RESTAPIEnabled=True,RESTAPIPort=8212)\n"
        );
        // The last member out leaves an empty struct, not a missing key.
        let only = "[S]\nK=(A=1)\n";
        assert_eq!(
            merge(only, &[], &["[S]K(A)".into()]).unwrap(),
            "[S]\nK=()\n"
        );
    }

    #[test]
    fn nested_lists_and_quoted_commas_survive_a_member_change() {
        let existing = "[S]\nOpts=(CrossplayPlatforms=(Steam,Xbox,PS5,Mac),DenyTechnologyList=(\"A\",\"B\"),ServerDescription=\"one, two (three)\",Difficulty=None,ExpRate=1.000000)\n";
        let out = merge(
            existing,
            &set(&[("[S]Opts(Difficulty)", json!("Hard"))]),
            &[],
        )
        .unwrap();
        assert_eq!(
            out,
            "[S]\nOpts=(CrossplayPlatforms=(Steam,Xbox,PS5,Mac),DenyTechnologyList=(\"A\",\"B\"),ServerDescription=\"one, two (three)\",Difficulty=\"Hard\",ExpRate=1.000000)\n"
        );
    }

    #[test]
    fn values_keep_their_type() {
        let out = merge(
            "",
            &set(&[
                ("[S]Plain", json!("a b")),
                ("[S]Flag", json!(true)),
                ("[S]Rate", json!(1.5)),
                ("[S]K(Name)", json!("a b")),
                ("[S]K(Count)", json!(10)),
                ("[S]K(On)", json!(false)),
            ]),
            &[],
        )
        .unwrap();
        assert_eq!(
            out,
            "[S]\nPlain=a b\nFlag=True\nRate=1.5\nK=(Name=\"a b\",Count=10,On=False)\n"
        );
    }

    /// A `"` in a struct string ends it early, and the rest becomes members.
    #[test]
    fn text_that_could_break_out_of_a_struct_string_is_refused() {
        for value in ["x\",RCONEnabled=True,A=\"", "trailing\\"] {
            let err = merge(PAL, &set(&[(&member("ServerName"), json!(value))]), &[]).unwrap_err();
            assert!(matches!(err, Error::Unwritable { .. }), "{err:?}");
            // The message names the key and never the value: it may be a password.
            assert!(!err.to_string().contains(value), "{err}");
        }
        // On a plain line a quote is only text.
        assert!(merge("", &set(&[("[S]K", json!("say \"hi\""))]), &[]).is_ok());
    }

    #[test]
    fn a_line_break_is_refused_anywhere() {
        for key in ["[S]K", "[S]K(M)"] {
            let err = merge("", &set(&[(key, json!("a\nB=1"))]), &[]).unwrap_err();
            assert!(matches!(err, Error::Unwritable { .. }), "{err:?}");
        }
        assert!(merge("", &set(&[("[S]K", json!("a\tb"))]), &[]).is_err());
    }

    #[test]
    fn a_value_that_is_not_a_scalar_is_refused() {
        assert!(merge("", &set(&[("[S]K", json!([1]))]), &[]).is_err());
        assert!(merge("", &set(&[("[S]K", json!({}))]), &[]).is_err());
    }

    /// Engine.ini, which the server writes on its first start: a new section
    /// is appended and everything already there is left exactly as it was.
    #[test]
    fn a_missing_section_is_appended_to_a_file_the_game_wrote() {
        let engine = "[Core.System]\r\nPaths=../../../Engine/Content\r\n; a comment\r\n\r\n[/Script/Engine.Engine]\r\nbSmoothFrameRate=True\r\n\r\n";
        let out = merge(
            engine,
            &set(&[(
                "[HTTPServer.Listeners]DefaultBindAddress",
                json!("127.0.0.1"),
            )]),
            &[],
        )
        .unwrap();
        assert_eq!(
            out,
            "[Core.System]\r\nPaths=../../../Engine/Content\r\n; a comment\r\n\r\n[/Script/Engine.Engine]\r\nbSmoothFrameRate=True\r\n\r\n[HTTPServer.Listeners]\r\nDefaultBindAddress=127.0.0.1\r\n"
        );
        assert_eq!(
            merge(
                &out,
                &set(&[(
                    "[HTTPServer.Listeners]DefaultBindAddress",
                    json!("127.0.0.1")
                )]),
                &[]
            )
            .unwrap(),
            out,
            "merging twice must not churn the file"
        );
    }

    #[test]
    fn a_key_is_replaced_where_it_sits_or_added_to_the_end_of_its_section() {
        let existing = "[A]\nx=1\nDefaultBindAddress=0.0.0.0\ny=2\n\n[B]\nz=3\n";
        let out = merge(
            existing,
            &set(&[
                ("[A]DefaultBindAddress", json!("127.0.0.1")),
                ("[A]New", json!("n")),
            ]),
            &[],
        )
        .unwrap();
        assert_eq!(
            out,
            "[A]\nx=1\nDefaultBindAddress=127.0.0.1\ny=2\nNew=n\n\n[B]\nz=3\n"
        );
    }

    #[test]
    fn keys_in_other_sections_and_array_lines_are_not_the_managed_key() {
        let existing = "[A]\nK=other\n+K=appended\n;K=commented\n[B]\nK=mine\n";
        let out = merge(existing, &set(&[("[B]K", json!("new"))]), &[]).unwrap();
        assert_eq!(out, "[A]\nK=other\n+K=appended\n;K=commented\n[B]\nK=new\n");
    }

    #[test]
    fn names_match_ignoring_case_as_unreal_does() {
        let out = merge("[a.b]\nkey=old\n", &set(&[("[A.B]Key", json!("new"))]), &[]).unwrap();
        assert_eq!(out, "[a.b]\nkey=new\n");
    }

    #[test]
    fn an_unset_plain_key_goes_and_takes_nothing_else_with_it() {
        let existing = "[S]\n; about K\nA=1\nK=secret\nB=2\n";
        assert_eq!(
            merge(existing, &[], &["[S]K".into(), "[T]K".into()]).unwrap(),
            "[S]\n; about K\nA=1\nB=2\n"
        );
    }

    #[test]
    fn a_duplicated_managed_key_is_refused_rather_than_guessed() {
        let err = merge("[S]\nK=1\nK=2\n", &set(&[("[S]K", json!("3"))]), &[]).unwrap_err();
        assert_eq!(err, Error::Duplicate("[S]K".into()));
        // Split across two headers of the same section is still twice.
        assert!(merge("[S]\nK=1\n[T]\n[S]\nK=2\n", &[], &["[S]K".into()]).is_err());
    }

    #[test]
    fn a_struct_this_cannot_read_is_refused_not_overwritten() {
        for value in ["(A=1", "(A=\"1)", "plain", "(A=1)trailing", "(A=1,B)"] {
            let existing = format!("[S]\nK={value}\n");
            let err = merge(&existing, &set(&[("[S]K(A)", json!(2))]), &[]).unwrap_err();
            assert_eq!(err, Error::NotAStruct("[S]K(A)".into()), "{value}");
        }
    }

    #[test]
    fn decoding_reads_what_unreal_writes_and_encoding_writes_it_back() {
        let text = "[S]\r\nName=Café\r\n";
        for encoding in [Encoding::Utf8, Encoding::Utf8Bom, Encoding::Utf16Le] {
            let bytes = encode(text, encoding);
            assert_eq!(decode(&bytes), Some((text.to_string(), encoding)));
        }
        assert_eq!(decode(&[0xFF, 0xFE, 0x41]), None);
        assert_eq!(decode(&[0xC3]), None);
    }
}
