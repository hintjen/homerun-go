//! Game extensions — the pure half.
//!
//! # What an extension is
//!
//! A descriptor is data, and most games need nothing else. Some need a
//! little code that is theirs alone: a vendor's own sign-in API, a token
//! that rotates, a console flow only that game prints. An extension is where
//! that code lives, so it does not leak into the engine as a descriptor field
//! with one user, into the protocol as a per-game event, or into the desktop
//! as a per-game screen.
//!
//! Each extension has two halves, the same split as the rest of the engine:
//!
//! | Half | Lives in | Holds |
//! |---|---|---|
//! | Pure | here, one module per extension | its [`ExtensionSpec`]: config validation, the values it supplies, the hosts it may reach, and any decision two hosts could answer differently |
//! | Effects | the runner, `homerun-game-cli/src/extensions/` | the hooks: HTTP, waiting on a person, console commands |
//!
//! A descriptor turns one on by name (`extension.name`) and passes it data
//! (`extension.config`). It cannot supply code: an extension is compiled into
//! the signed runner, and the descriptor only chooses among the ones that
//! are. That is the same line `console.rcon.protocol` already draws.
//!
//! # When something belongs in an extension
//!
//! The schema grows when a **second** game needs the same thing. Until then,
//! the need lives in the first game's extension, and when the second game
//! arrives it moves into a primitive or a descriptor field, with the first
//! extension switched over in the same change. A field with one user is how
//! an engine fills up with special cases nobody can remove.
//!
//! # What reaches the launch
//!
//! An extension's output reaches a server through one placeholder,
//! `{extension:<key>}`, whose keys the spec declares in [`ExtensionSpec::supplies`].
//! It is not `{secret:…}` on purpose: the host generates every `{secret:…}`
//! ([`super::validate::required_secrets`]), and an extension's values are
//! not the host's to generate. A value marked secret may appear only in
//! `launch.env` — argv is visible to every process on the machine, and a
//! config file lives in a folder that is backed up.

use serde_json::Value;

use super::descriptor::GameDescriptor;
use super::validate::Report;

#[cfg(any(test, feature = "test-extensions"))]
pub mod fixture;

/// The pure half of one extension. See the module header.
#[derive(Debug, Clone, Copy)]
pub struct ExtensionSpec {
    /// What a descriptor names in `extension.name`. `[a-z0-9-]+`.
    pub name: &'static str,
    /// Every key `{extension:<key>}` may name, and which values are secret.
    pub supplies: &'static [Supply],
    /// Adds problems and warnings about the descriptor's `extension.config`.
    ///
    /// Called only with an object: a config that is not one has already been
    /// refused. Messages are for whoever writes the descriptor.
    pub validate: fn(config: &Value, descriptor: &GameDescriptor, report: &mut Report),
    /// The hosts the extension may reach over HTTPS, and send a person to
    /// for sign-in, read from a config that has passed [`Self::validate`].
    ///
    /// The runner enforces this list; the extension never checks it itself.
    pub hosts: fn(config: &Value) -> Vec<String>,
    /// JSON Schema for `extension.config`, spliced into the descriptor
    /// schema under this extension's name.
    pub config_schema: fn() -> Value,
}

/// One value an extension supplies to the launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Supply {
    pub key: &'static str,
    /// Redacted from every log, and allowed only in `launch.env`.
    pub secret: bool,
}

impl ExtensionSpec {
    /// The declared supply for `key`, if there is one.
    pub fn supply(&self, key: &str) -> Option<&'static Supply> {
        self.supplies.iter().find(|s| s.key == key)
    }

    /// The feature name a runner advertises when it has this extension.
    pub fn feature(&self) -> String {
        feature(self.name)
    }
}

/// The feature name for an extension called `name`.
pub fn feature(name: &str) -> String {
    format!("extension:{name}")
}

/// The feature a runner advertises when it has the extension mechanism at
/// all: the `{extension:…}` placeholder, the generic events and commands.
pub const FEATURE: &str = "extensions";

/// Extensions a release build carries, and the only ones the published
/// schema describes.
///
/// Empty until the first real extension lands: this change is the interface.
pub const PUBLISHED: &[ExtensionSpec] = &[];

/// Every extension this build can run: [`PUBLISHED`], plus the test-only
/// reference extension when built for tests.
pub fn registry() -> Vec<&'static ExtensionSpec> {
    #[allow(unused_mut)]
    let mut all: Vec<&'static ExtensionSpec> = PUBLISHED.iter().collect();
    #[cfg(any(test, feature = "test-extensions"))]
    all.push(&fixture::SPEC);
    all
}

/// The extension called `name`, if this build has it.
pub fn spec(name: &str) -> Option<&'static ExtensionSpec> {
    registry().into_iter().find(|s| s.name == name)
}

/// Whether `url` is an `https://` address on one of `hosts`, or a subdomain
/// of one.
///
/// The one check every URL an extension shows a person goes through, so a
/// sign-in link can never point somewhere the descriptor did not name --
/// whatever a vendor's program or a server's console printed. Deliberately
/// strict: no userinfo (`https://trusted@evil`), no port, no other scheme,
/// and the host compared case-insensitively on whole labels, so
/// `hytale.com.evil.net` and `evilhytale.com` are both refused for
/// `hytale.com`.
pub fn url_allowed(url: &str, hosts: &[String]) -> bool {
    let Some(rest) = strip_prefix_ignore_case(url, "https://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains(['@', ':', '\\']) {
        return false;
    }
    let host = authority.to_ascii_lowercase();
    hosts.iter().any(|allowed| {
        let allowed = allowed.trim().to_ascii_lowercase();
        !allowed.is_empty()
            && (host == allowed
                || host
                    .strip_suffix(&allowed)
                    .is_some_and(|prefix| prefix.ends_with('.')))
    })
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

/// Whether `name` is a well-formed extension name.
pub fn name_is_valid(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn hosts(list: &[&str]) -> Vec<String> {
        list.iter().map(|h| h.to_string()).collect()
    }

    #[test]
    fn a_url_on_an_allowed_host_or_its_subdomain_is_allowed() {
        let allowed = hosts(&["hytale.com"]);
        for url in [
            "https://hytale.com",
            "https://hytale.com/",
            "https://oauth.accounts.hytale.com/oauth2/device/verify?user_code=AB-12",
            "HTTPS://Accounts.Hytale.COM/device",
            "https://accounts.hytale.com#frag",
        ] {
            assert!(url_allowed(url, &allowed), "{url}");
        }
    }

    #[test]
    fn a_url_anywhere_else_is_refused() {
        let allowed = hosts(&["hytale.com"]);
        for url in [
            "http://accounts.hytale.com/device",
            "https://hytale.com.evil.net/device",
            "https://evilhytale.com/device",
            "https://hytale.com@evil.net/device",
            // Lands on an allowed host, but shows a person another site's
            // name first: a sign-in link must not read like a phishing one.
            "https://evil.net@accounts.hytale.com/device",
            "https://evil.net/?next=https://hytale.com",
            "https://hytale.com:8443/device",
            "https://hytale.com\\@evil.net",
            "javascript:alert(1)",
            "https://",
            "",
            "hytale.com",
        ] {
            assert!(!url_allowed(url, &allowed), "{url}");
        }
    }

    #[test]
    fn no_hosts_allows_nothing() {
        assert!(!url_allowed("https://hytale.com", &[]));
        assert!(!url_allowed("https://hytale.com", &hosts(&["", "  "])));
    }

    #[test]
    fn every_registered_extension_is_well_formed() {
        let mut names = HashSet::new();
        for spec in registry() {
            assert!(name_is_valid(spec.name), "{}", spec.name);
            assert!(names.insert(spec.name), "{} registered twice", spec.name);
            let mut keys = HashSet::new();
            for supply in spec.supplies {
                assert!(
                    !supply.key.is_empty() && !supply.key.contains(['{', '}', ':']),
                    "{}: {:?}",
                    spec.name,
                    supply.key
                );
                assert!(
                    keys.insert(supply.key),
                    "{} supplies {} twice",
                    spec.name,
                    supply.key
                );
            }
            assert!(
                (spec.config_schema)().is_object(),
                "{}'s config schema is not an object",
                spec.name
            );
        }
    }

    #[test]
    fn the_fixture_is_registered_for_tests_and_never_published() {
        assert!(spec("fixture").is_some());
        assert!(PUBLISHED.iter().all(|s| s.name != "fixture"));
        assert!(spec("nope").is_none());
    }

    #[test]
    fn feature_names_follow_one_spelling() {
        assert_eq!(fixture::SPEC.feature(), "extension:fixture");
        assert_eq!(FEATURE, "extensions");
    }
}
