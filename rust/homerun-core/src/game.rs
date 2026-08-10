//! What a hostable game has to expose, and nothing about any game in
//! particular.
//!
//! # Why this exists
//!
//! Everything a Homerun host does around a server is the same whatever the
//! server is: fetch a thing to run, write config before starting it, spawn it,
//! read its output to know when it is ready and who joined, expose ports
//! through the gateway, and decide whether an exit was a crash. Only the
//! *answers* are game-specific.
//!
//! So the answers live behind [`Game`], and Minecraft is one implementation of
//! it ([`crate::minecraft`]). Adding a second game means adding an
//! implementation and a registry entry — not editing the tunnel, the state
//! machine, the property merge, or any host.
//!
//! # What deliberately is not here
//!
//! **Artifact resolution.** Minecraft resolves a jar from Mojang's manifest;
//! another game might resolve a Steam depot, a container image, or nothing at
//! all because it ships in the app. Those have no honest common signature, and
//! forcing one would produce a trait method every implementation ignores half
//! the arguments of. Each game exposes its own resolution under its own module
//! and its own bridge namespace; the generic layer never calls it.
//!
//! That line is worth holding: this trait covers what hosts genuinely do
//! uniformly, and stops there.

use crate::tunnel::Forward;
use crate::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What one line of a server's output means, if anything.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LineMeaning {
    /// The server has finished starting and is accepting connections.
    ///
    /// Not the same as a host reporting `running`, which also waits for the
    /// tunnel — a server nobody can reach is not up in any sense a player
    /// cares about.
    pub ready: bool,
    pub joined: Option<String>,
    pub left: Option<String>,
}

/// How a config file must be written.
///
/// Carried with the contents so a host never has to know that one game's
/// config is latin-1 and another's is UTF-8. Getting this wrong is invisible
/// until a player sees mojibake in a server list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Encoding {
    Utf8,
    Latin1,
}

/// A file to write before the server starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileWrite {
    /// Relative to the server's directory. Never absolute, never traversing.
    pub path: String,
    pub contents: String,
    pub encoding: Encoding,
}

/// A file the game needs to read before it can decide what to write.
///
/// The encoding is here and not only on [`FileWrite`] because reading is just
/// as easy to get wrong: decoding a latin-1 `server.properties` as UTF-8 turns
/// `§` into a replacement character, and writing that back as latin-1 turns it
/// into `?`. A MOTD's colour codes are destroyed by a launch that changed
/// nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigInput {
    /// Relative to the server's directory. Never absolute, never traversing.
    pub path: String,
    pub encoding: Encoding,
}

/// A player whose identity only the network can supply.
///
/// **Only names the game cannot resolve itself appear here.** An identity a
/// game can derive — Minecraft's offline UUID is an MD5 of the name — is
/// derived inside [`Game::config_files`] and never asked for, because pushing
/// that derivation onto the host would put game-specific knowledge back in
/// every platform. An offline-mode server therefore returns nothing here and
/// makes no requests at all.
///
/// A name the host fails to resolve is simply left out of
/// [`BuildContext::resolved`], and the game decides what that means. Minecraft
/// skips it: an entry written with a wrong id sits in the file looking correct
/// and never matches the player.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lookup {
    pub name: String,
}

/// A player resolved to the identity the server will match them by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub name: String,
    pub id: String,
}

/// Everything [`Game::config_files`] needs that only the host knows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildContext {
    /// The server's settings as the API expressed them.
    pub env: Value,
    /// The API's game type verbatim, before any host reduces it.
    #[serde(default)]
    pub game_type: String,
    /// The port the server will actually bind, after conflict resolution.
    pub port: u16,
    /// `127.0.0.1`, or `0.0.0.0` when the user opted into LAN exposure.
    pub bind_address: String,
    /// Existing contents of files the game asked to read, keyed by path.
    /// A file that does not exist is simply absent.
    #[serde(default)]
    pub existing: std::collections::BTreeMap<String, String>,
    /// The identities the host resolved for [`Game::required_lookups`].
    #[serde(default)]
    pub resolved: Vec<Identity>,
    /// A timestamp in whatever format the game asked for. Passed in because
    /// this crate has no clock — that is what keeps it deterministic.
    #[serde(default)]
    pub now: String,
}

impl BuildContext {
    pub fn existing(&self, path: &str) -> &str {
        self.existing.get(path).map(String::as_str).unwrap_or("")
    }

    pub fn identity(&self, name: &str) -> Option<&Identity> {
        self.resolved.iter().find(|i| i.name == name)
    }
}

/// A game a Homerun host can run.
pub trait Game: Sync {
    /// Stable identifier, used by the bridge and by stored state.
    fn id(&self) -> &'static str;

    /// What one line of server output means.
    fn classify(&self, line: &str) -> LineMeaning;

    /// Which files the game will want to read in [`Game::config_files`], and
    /// how to decode each.
    ///
    /// Declared up front so the host reads them once rather than the trait
    /// needing filesystem access. A file that does not exist is simply absent
    /// from [`BuildContext::existing`] — never an error.
    fn config_inputs(&self, env: &Value) -> Vec<ConfigInput>;

    /// Players whose identity the host must fetch before config can be
    /// written. See [`Lookup`] — this is only what the game cannot derive.
    /// `game_type` is the API's verbatim, as on [`BuildContext`] — it can
    /// change the answer (Minecraft's crossplay mode resolves offline), so
    /// asking without it would disagree with [`Game::config_files`].
    fn required_lookups(&self, env: &Value, game_type: &str) -> Vec<Lookup>;

    /// The files to write before starting, given everything gathered.
    fn config_files(&self, ctx: &BuildContext) -> Result<Vec<FileWrite>>;

    /// Which ports to carry through the gateway, and how.
    ///
    /// `exposure` names a mode the game defines; `port` is the local port the
    /// server bound. An unknown mode must be an error, never a default —
    /// silently producing the wrong forwards yields a server that runs and is
    /// unreachable.
    fn forwards(&self, exposure: &str, port: u16, extra: &Value) -> Result<Vec<Forward>>;
}

/// Every game this build can host.
pub fn all() -> &'static [&'static dyn Game] {
    &[&crate::minecraft::Minecraft]
}

/// Look a game up by [`Game::id`].
pub fn by_id(id: &str) -> Option<&'static dyn Game> {
    all().iter().copied().find(|g| g.id() == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_game_is_reachable_by_its_own_id() {
        for game in all() {
            assert!(
                by_id(game.id()).is_some(),
                "{} is registered but not findable",
                game.id()
            );
        }
    }

    #[test]
    fn game_ids_are_unique() {
        let mut seen: Vec<&str> = Vec::new();
        for game in all() {
            assert!(!seen.contains(&game.id()), "duplicate id {}", game.id());
            seen.push(game.id());
        }
    }

    #[test]
    fn an_unknown_game_is_none_rather_than_a_default() {
        assert!(by_id("halflife").is_none());
        assert!(by_id("").is_none());
    }

    /// The exact JSON `Core.configFiles` builds in Kotlin.
    ///
    /// Worth pinning because the failure mode is silent: a host swallows a
    /// deserialisation error and falls back to the server's own defaults, so a
    /// renamed field looks exactly like "settings not configured" rather than
    /// like a bug. `game_type` and `bind_address` are snake_case on the wire
    /// while their neighbours in the same object are not, which is precisely
    /// the sort of thing that gets quietly "tidied up".
    #[test]
    fn the_hosts_build_context_json_deserialises() {
        let wire = serde_json::json!({
            "env": { "MOTD": "from the wire", "OPS": "Notch" },
            "game_type": "native-crossplay",
            "port": 25570,
            "bind_address": "127.0.0.1",
            "existing": { "server.properties": "rate-limit=0\n" },
            "resolved": [{ "name": "Notch", "id": "b50ad385-829d-3141-a216-7e7d7539ba7f" }],
            "now": "2026-08-10 14:03:22 +0000"
        });

        let ctx: BuildContext = serde_json::from_value(wire).expect("the host's shape must parse");
        assert_eq!(ctx.game_type, "native-crossplay");
        assert_eq!(ctx.port, 25570);
        assert_eq!(ctx.bind_address, "127.0.0.1");
        assert_eq!(ctx.existing("server.properties"), "rate-limit=0\n");
        assert_eq!(
            ctx.identity("Notch").map(|i| i.id.as_str()),
            Some("b50ad385-829d-3141-a216-7e7d7539ba7f")
        );

        // And it survives a full round trip through the trait.
        let files = crate::minecraft::Minecraft.config_files(&ctx).unwrap();
        assert!(files.iter().any(|f| f.contents.contains("from the wire")));
    }

    /// The host reads these back off each file; a renamed variant would make
    /// every latin-1 file silently UTF-8.
    #[test]
    fn encodings_serialise_as_the_host_reads_them() {
        assert_eq!(serde_json::to_value(Encoding::Latin1).unwrap(), "latin1");
        assert_eq!(serde_json::to_value(Encoding::Utf8).unwrap(), "utf8");
    }

    /// Optional fields must stay optional: a host that has read no files and
    /// resolved nobody sends neither key.
    #[test]
    fn a_minimal_context_parses() {
        let ctx: BuildContext = serde_json::from_value(serde_json::json!({
            "env": {},
            "port": 25565,
            "bind_address": "127.0.0.1"
        }))
        .expect("optional fields must be optional");
        assert!(ctx.existing.is_empty());
        assert!(ctx.resolved.is_empty());
    }

    #[test]
    fn a_context_reports_absent_files_as_empty_rather_than_failing() {
        let ctx = BuildContext {
            env: serde_json::json!({}),
            game_type: "java".into(),
            port: 25565,
            bind_address: "127.0.0.1".into(),
            existing: Default::default(),
            resolved: vec![],
            now: String::new(),
        };
        assert_eq!(ctx.existing("never-written.json"), "");
        assert!(ctx.identity("Nobody").is_none());
    }
}
