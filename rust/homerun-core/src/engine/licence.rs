//! The gate in front of every download and every launch.
//!
//! # A session never accepts on anyone's behalf
//!
//! This is a rule about people, enforced in code. The *user* accepts each
//! game's terms, once, at create time — the way Minecraft's EULA is handled
//! today — and that acceptance is recorded with the licence URL and a
//! timestamp so it is later recoverable what was agreed to.
//!
//! Nothing here infers acceptance. Not from a descriptor, not from a previous
//! server, not from the fact that the runtime is already on disk. The runner
//! is handed `licenceAccepted` explicitly and refuses `fetch` and `start`
//! without it; on the CLI the person running the command passes a flag that
//! names what they are accepting. An agent driving either one must stop and
//! ask a human instead — and if a vendor's own installer prompts for terms,
//! that is a stop, not a prompt to answer.
//!
//! # Absent is not accepted
//!
//! `licence: null` means the game has no terms of its own to agree to. It
//! never means terms that were agreed to. Those are different claims and the
//! difference is the whole point of the module.

use serde::{Deserialize, Serialize};

use super::descriptor::GameDescriptor;
use crate::game::{Encoding, FileWrite};
use crate::{Error, Result};

/// The code the `supervise` protocol carries when this gate refuses.
pub const REFUSED_CODE: &str = "licence_not_accepted";

/// What the terms are, for a caller that has to show them before asking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Terms {
    pub name: String,
    pub url: String,
}

/// The terms a person must accept before this game does anything, if any.
pub fn terms(descriptor: &GameDescriptor) -> Option<Terms> {
    descriptor.licence.as_ref().map(|l| Terms {
        name: l.name.clone(),
        url: l.url.clone(),
    })
}

/// Refuse unless this game's terms have been accepted.
///
/// Returns the files the *game itself* wants written as proof — Minecraft's
/// `eula.txt` is the precedent — which is empty for a game that needs none.
///
/// `accepted` is the host's record of a person's decision. It is never
/// defaulted to `true` anywhere, and a caller that has no record passes
/// `false`.
pub fn gate(descriptor: &GameDescriptor, accepted: bool) -> Result<Vec<FileWrite>> {
    let Some(licence) = descriptor.licence.as_ref() else {
        // No terms of its own. Nothing to accept, nothing to write.
        return Ok(Vec::new());
    };

    if !accepted {
        return Err(Error::Unsupported(format!(
            "{} cannot be downloaded or started until you have accepted {} at {}.",
            display_name(descriptor),
            licence.name,
            licence.url
        )));
    }

    Ok(match &licence.accept_via {
        Some(via) => vec![FileWrite {
            path: via.file.clone(),
            contents: via.contents.clone(),
            // The games in reach write ASCII here. Stated rather than
            // assumed, because `game::Encoding` exists precisely because
            // guessing this wrong is invisible until a player sees mojibake.
            encoding: Encoding::Utf8,
        }],
        None => Vec::new(),
    })
}

fn display_name(descriptor: &GameDescriptor) -> String {
    if descriptor.name.is_empty() {
        "This game".to_string()
    } else {
        descriptor.name.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rust() -> GameDescriptor {
        GameDescriptor::parse(include_str!("testdata/rust.json")).unwrap()
    }

    #[test]
    fn the_pilot_is_refused_until_someone_has_accepted_its_terms() {
        let err = gate(&rust(), false).unwrap_err().to_string();
        assert!(err.contains("Rust"), "names the game: {err}");
        assert!(err.contains("Facepunch"), "names the terms: {err}");
        assert!(
            err.contains("https://store.steampowered.com/subscriber_agreement/"),
            "gives the person somewhere to read them: {err}"
        );
    }

    #[test]
    fn once_accepted_it_asks_for_nothing_to_be_written() {
        assert!(gate(&rust(), true).unwrap().is_empty());
    }

    /// The Minecraft precedent: some games want the acceptance in a file of
    /// their own before they will boot.
    #[test]
    fn a_game_that_wants_proof_on_disk_gets_it_written_only_after_acceptance() {
        let d: GameDescriptor = serde_json::from_value(json!({
            "id": "g", "name": "G",
            "licence": { "name": "The EULA", "url": "https://x/eula",
                         "acceptVia": { "file": "eula.txt", "contents": "eula=true\n" } }
        }))
        .unwrap();

        assert!(
            gate(&d, false).is_err(),
            "nothing is written before acceptance"
        );

        let files = gate(&d, true).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "eula.txt");
        assert_eq!(files[0].contents, "eula=true\n");
        assert_eq!(files[0].encoding, Encoding::Utf8);
    }

    /// The distinction the module exists to hold: a game with no terms is not
    /// a game whose terms were accepted, but it does have nothing to refuse.
    #[test]
    fn a_game_with_no_terms_of_its_own_needs_no_acceptance() {
        let d: GameDescriptor = serde_json::from_value(json!({ "id": "g" })).unwrap();
        assert!(gate(&d, false).unwrap().is_empty());
        assert!(terms(&d).is_none());
    }

    #[test]
    fn the_terms_are_offered_so_a_caller_can_show_them_before_asking() {
        let t = terms(&rust()).expect("the pilot has terms");
        assert_eq!(t.name, "Facepunch / Steam Subscriber Agreement");
        assert!(t.url.starts_with("https://"));
    }

    /// The refusal a player sees must be a sentence, and the code the
    /// protocol carries must be the one the contract names.
    #[test]
    fn the_refusal_reads_as_a_verdict_and_has_a_stable_code() {
        assert_eq!(REFUSED_CODE, "licence_not_accepted");
        let err = gate(&rust(), false).unwrap_err().to_string();
        for forbidden in ["unwrap", "panicked", "Err(", "errno"] {
            assert!(!err.contains(forbidden), "{err}");
        }
    }
}
