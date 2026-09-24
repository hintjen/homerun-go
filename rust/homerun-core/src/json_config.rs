//! Merging managed keys into a game's JSON configuration file.
//!
//! The JSON counterpart of [`crate::properties`], with the same contract:
//! anything the host does not manage survives, only the managed keys change,
//! and an unset setting takes its key *out* of the file rather than leaving
//! the previous launch's value behind.
//!
//! # Paths, not just keys
//!
//! A managed key is a path: `"Defaults.GameMode"` is the `GameMode` member of
//! the `Defaults` object. Games that keep their settings in JSON nest them —
//! Hytale's server keeps the default game mode one level down — and a
//! descriptor that could only reach the top level could not manage them at
//! all. Missing objects along the way are created. A path that runs into
//! something that is not an object is refused rather than overwritten: that
//! value is the game's, and replacing it with an object would be a guess about
//! a file format this code does not own.
//!
//! A key with a literal `.` in its name therefore cannot be managed. No game
//! needs one yet; when one does, that is an escape syntax, not a second rule.
//!
//! # Values keep their type
//!
//! The caller passes `serde_json::Value`s, not strings, because a game that
//! reads `"MaxPlayers": "10"` is entitled to refuse it — and Hytale's does, at
//! config load, before the server has printed anything a host could act on.
//! Deciding *which* type a value has is [`crate::engine::template::fill_value`]'s
//! business; this module only puts it where it belongs.
//!
//! # Order
//!
//! Members come out in key order. `serde_json` is built without
//! `preserve_order` in this workspace, deliberately, because turning it on
//! would reorder every map in every crate — the committed descriptor schema
//! included. Game servers that own their JSON (Hytale's does) rewrite it on
//! their next save anyway.

use serde_json::{Map, Value};
use std::fmt;

/// Why a JSON configuration file could not be merged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The existing file is not a JSON object at its top level.
    NotAnObject,
    /// A managed path runs through a member that exists and is not an object.
    NotAContainer(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotAnObject => write!(f, "the existing game configuration is not a JSON object"),
            Error::NotAContainer(path) => write!(
                f,
                "the game configuration has a value at \"{path}\" where a group of settings was expected"
            ),
        }
    }
}

impl std::error::Error for Error {}

/// The segments of a managed key, or `None` if the key is not a usable path.
///
/// Empty segments (`"a..b"`, `".a"`, `"a."`) are not paths into anything, and
/// [`crate::engine::validate`] refuses them in a descriptor before a launch
/// ever gets here.
pub fn segments(key: &str) -> Option<Vec<&str>> {
    let parts: Vec<&str> = key.split('.').collect();
    if parts.iter().any(|p| p.is_empty()) {
        None
    } else {
        Some(parts)
    }
}

/// Merge managed values into an existing file.
///
/// `existing` may be empty, which produces a file of only the managed keys.
/// `clear` names keys whose setting is unset: each is removed if present, and
/// objects it leaves empty are kept (the game may expect them).
pub fn merge(existing: &str, set: &[(String, Value)], clear: &[String]) -> Result<String, Error> {
    let mut root = if existing.trim().is_empty() {
        Map::new()
    } else {
        match serde_json::from_str::<Value>(existing) {
            Ok(Value::Object(map)) => map,
            _ => return Err(Error::NotAnObject),
        }
    };
    for key in clear {
        if let Some(parts) = segments(key) {
            remove(&mut root, &parts);
        }
    }
    for (key, value) in set {
        // An unusable path is a descriptor defect validate reports; skipping
        // it here keeps a stale descriptor from corrupting the file.
        let Some(parts) = segments(key) else { continue };
        insert(&mut root, &parts, value.clone())?;
    }
    let mut text =
        serde_json::to_string_pretty(&Value::Object(root)).expect("a JSON value always serialises");
    text.push('\n');
    Ok(text)
}

fn insert(root: &mut Map<String, Value>, parts: &[&str], value: Value) -> Result<(), Error> {
    let (last, parents) = parts.split_last().expect("segments is never empty");
    let mut here = root;
    for (i, part) in parents.iter().enumerate() {
        let slot = here
            .entry((*part).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        here = match slot {
            Value::Object(map) => map,
            _ => return Err(Error::NotAContainer(parts[..=i].join("."))),
        };
    }
    here.insert((*last).to_string(), value);
    Ok(())
}

fn remove(root: &mut Map<String, Value>, parts: &[&str]) {
    let (last, parents) = parts.split_last().expect("segments is never empty");
    let mut here = root;
    for part in parents {
        match here.get_mut(*part) {
            Some(Value::Object(map)) => here = map,
            _ => return,
        }
    }
    here.remove(*last);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn set(pairs: &[(&str, Value)]) -> Vec<(String, Value)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn parsed(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn numbers_and_booleans_are_written_as_themselves_not_as_strings() {
        // The Hytale failure: "MaxPlayers": "10" stopped the server at config load.
        let out = merge(
            "",
            &set(&[("MaxPlayers", json!(10)), ("Pvp", json!(true))]),
            &[],
        )
        .unwrap();
        assert_eq!(parsed(&out), json!({ "MaxPlayers": 10, "Pvp": true }));
    }

    #[test]
    fn a_dotted_key_reaches_into_nested_objects_and_creates_missing_ones() {
        let out = merge("", &set(&[("Defaults.GameMode", json!("Creative"))]), &[]).unwrap();
        assert_eq!(
            parsed(&out),
            json!({ "Defaults": { "GameMode": "Creative" } })
        );
    }

    #[test]
    fn unmanaged_members_survive_including_siblings_of_a_managed_one() {
        let existing =
            r#"{ "Version": 3, "Defaults": { "World": "default", "GameMode": "Adventure" } }"#;
        let out = merge(
            existing,
            &set(&[("Defaults.GameMode", json!("Creative"))]),
            &[],
        )
        .unwrap();
        assert_eq!(
            parsed(&out),
            json!({ "Version": 3, "Defaults": { "World": "default", "GameMode": "Creative" } })
        );
    }

    #[test]
    fn an_unset_setting_takes_its_key_out_of_the_file() {
        let existing =
            r#"{ "Password": "old", "Defaults": { "GameMode": "Creative", "World": "w" } }"#;
        let out = merge(
            existing,
            &[],
            &["Password".into(), "Defaults.GameMode".into()],
        )
        .unwrap();
        assert_eq!(parsed(&out), json!({ "Defaults": { "World": "w" } }));
    }

    #[test]
    fn clearing_a_key_that_is_not_there_is_not_an_error() {
        let out = merge(r#"{"a":1}"#, &[], &["b.c".into(), "a.b".into()]).unwrap();
        assert_eq!(parsed(&out), json!({ "a": 1 }));
    }

    #[test]
    fn a_path_through_a_non_object_is_refused_not_overwritten() {
        let err = merge(
            r#"{ "Defaults": "flat" }"#,
            &set(&[("Defaults.GameMode", json!("x"))]),
            &[],
        )
        .unwrap_err();
        assert_eq!(err, Error::NotAContainer("Defaults".into()));
    }

    #[test]
    fn a_file_that_is_not_an_object_is_refused() {
        assert_eq!(merge("[1,2]", &[], &[]).unwrap_err(), Error::NotAnObject);
        assert_eq!(merge("not json", &[], &[]).unwrap_err(), Error::NotAnObject);
    }

    #[test]
    fn a_string_that_looks_like_a_number_stays_a_string() {
        // Typing is the caller's decision; merge never guesses from the text.
        let out = merge("", &set(&[("ServerName", json!("10"))]), &[]).unwrap();
        assert_eq!(parsed(&out), json!({ "ServerName": "10" }));
    }

    #[test]
    fn empty_segments_are_not_paths() {
        assert!(segments("a..b").is_none());
        assert!(segments(".a").is_none());
        assert!(segments("a.").is_none());
        assert_eq!(segments("a.b"), Some(vec!["a", "b"]));
    }
}
