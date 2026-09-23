//! `game.json` — the descriptor, as Rust types.
//!
//! # These types *are* the schema
//!
//! There is no second spelling of a descriptor anywhere. The JSON Schema in
//! [`super::schema`] is generated from what is here, the API's registry is
//! generated from the file, and every other layer reads one of those two. A
//! field that is not in this file does not exist.
//!
//! # Two rules that look like carelessness and are not
//!
//! **Every field has a default.** Not because a descriptor may omit anything
//! — most of these are required and [`super::validate`] says so — but because
//! a missing field must fail as *a sentence a player can read*, and serde's
//! own message ("missing field `exe` at line 34 column 5") is not that. So
//! parsing accepts nearly everything and validation is the only gate. It is
//! also the only place the rules live, which is why they can be stated once
//! and tested exhaustively.
//!
//! **`deny_unknown_fields` is off.** A descriptor written against a newer
//! schema than this build knows must *degrade*, not fail: the host ignores
//! the key it has never heard of and runs the game. The alternative is that
//! adding an optional field to the schema breaks every host in the field that
//! has not been rebuilt — which is the same freeze the `Game` trait carries,
//! arrived at from the other direction.
//!
//! The one place that rule cannot hold is a tagged enum: serde has to pick a
//! variant before it can ignore anything. Those carry an explicit unknown
//! variant instead — see [`RuntimeSource`] — so a runtime source this build
//! cannot fetch is a readable refusal rather than a parse error.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tunnel::Protocol;

/// The schema version these types implement.
///
/// A descriptor declaring a *higher* number is refused by
/// [`super::validate`] — it may rely on a key this build does not read, and
/// running it anyway is how a player gets a server that starts and is
/// misconfigured. A descriptor declaring a lower one is fine: every field
/// gained since is optional by the rule above.
pub const SCHEMA_VERSION: u32 = 0;

/// One game, as the file on disk describes it.
///
/// The halves are marked because they have different trust: the **servable**
/// half is generated into the API and served to any UI, while the
/// **host-only** half decides which executable a machine downloads and runs
/// and is read only from the copy bundled into a signed host build.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameDescriptor {
    /// Schema version. See [`SCHEMA_VERSION`].
    #[serde(default)]
    pub schema: u32,
    /// Slug: `[a-z0-9-]+`. Also the API's game id and the `games/<id>/`
    /// directory name, so it is not cosmetic.
    #[serde(default)]
    pub id: String,
    /// What a player calls it.
    #[serde(default)]
    pub name: String,
    /// The host platforms this game can run on, **declared, never inferred**.
    ///
    /// Following `minecraft::hosting`'s rule that a host declares its engines
    /// so it is never offered a server it will fail to run. A phone is not a
    /// host for a descriptor-driven game at all — it cannot exec a downloaded
    /// binary — so this is `["win32-x64"]` for everything today.
    #[serde(default)]
    pub hosts: Vec<String>,

    // ---- servable half ----
    #[serde(default)]
    pub catalog: Catalog,
    /// The terms the *person hosting* accepts, once, at create time.
    ///
    /// `None` means the game has none to accept. It never means "accepted" —
    /// see [`super::licence`].
    #[serde(default)]
    pub licence: Option<Licence>,
    #[serde(default)]
    pub client: Client,
    /// What a player may change, and the bounds on each.
    #[serde(default)]
    pub settings: Vec<Setting>,

    // ---- host-only half ----
    #[serde(default)]
    pub requires: Requires,
    /// Keyed by host platform (`win32-x64`). Only where games genuinely
    /// differ: the artifact and the launch line. Everything else below is
    /// shared across platforms.
    #[serde(default)]
    pub platforms: BTreeMap<String, Platform>,
    #[serde(default)]
    pub ready: Ready,
    #[serde(default)]
    pub console: Console,
    #[serde(default)]
    pub stop: Stop,
    #[serde(default)]
    pub ports: Vec<Port>,
    #[serde(default)]
    pub config: Vec<ConfigFile>,
    #[serde(default)]
    pub saves: Saves,
    #[serde(default)]
    pub observe: Observe,
    #[serde(default)]
    pub mods: Mods,
    /// Reserved: per-game ceilings (storage, monthly transfer, server count).
    ///
    /// Deliberately untyped and deliberately present. The key exists so that
    /// adding limits later is not a schema break; nothing reads it yet.
    /// TODO(per-game-limits)
    #[serde(default)]
    pub limits: Value,
}

/// How the game is presented before anyone has one.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    #[serde(default)]
    pub blurb: String,
    /// `#RRGGBB`. Validated, because a malformed colour reaches a stylesheet.
    #[serde(default)]
    pub accent: String,
    /// Path inside the game module, relative — `art/card.png`.
    #[serde(default)]
    pub art: String,
}

/// Terms a person must accept before anything is downloaded.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Licence {
    pub name: String,
    /// The canonical URL of the terms. Stored with the acceptance, so it is
    /// recoverable later which text was agreed to.
    pub url: String,
    /// Every document the person is agreeing to, when the terms are more than
    /// one document.
    ///
    /// Some games' terms are a single page and `name` and `url` say all there
    /// is to say. Others are several: Rust's are the Facepunch Terms of
    /// Service, the Facepunch Community Server and Hosting Guidelines and the
    /// Steam Subscriber Agreement, three separately published documents with
    /// three URLs. Squeezed into one `{name, url}` pair, the names survive as
    /// prose and two of the three links are simply lost -- so what a person
    /// accepted cannot be reconstructed from what was recorded, which is the
    /// one thing an acceptance record exists to do.
    ///
    /// Empty means "the terms are the single document `name` and `url`
    /// describe", which is what every descriptor written before this field
    /// meant. When it is not empty, `name` and `url` stay the one-line summary
    /// a host shows where it has room for one, and `url` must be one of these
    /// documents -- a summary link that is not among the things being accepted
    /// is a fourth document nobody listed.
    #[serde(default)]
    pub documents: Vec<LicenceDocument>,
    /// What the *game* wants written once the person has accepted, if
    /// anything. Minecraft's `eula.txt` is the precedent. `None` means
    /// acceptance is recorded by us and nothing is written into the server.
    #[serde(default)]
    pub accept_via: Option<AcceptVia>,
}

/// One published document within a game's terms.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LicenceDocument {
    pub name: String,
    pub url: String,
}

/// A file the game itself reads as proof of acceptance.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcceptVia {
    /// Relative to the server directory.
    pub file: String,
    pub contents: String,
}

/// How a player joins.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Client {
    /// A deep link, templated with `{host}` and `{port:<name>}` — the Play
    /// button. `None` means the player copies an address.
    #[serde(default)]
    pub join_url: Option<String>,
    /// The SRV label to publish, when the game's client resolves SRV records
    /// — then a player gets a port-less name. `None` is not a defect; it
    /// means `host:port`, which is an accepted UX.
    #[serde(default)]
    pub srv: Option<String>,
}

/// What a setting is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SettingKind {
    #[default]
    String,
    Int,
    Bool,
}

/// One thing a player may change.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Setting {
    /// Referenced by `{setting:<key>}` and stored by the API under this name.
    #[serde(default)]
    pub key: String,
    #[serde(rename = "type", default)]
    pub kind: SettingKind,
    #[serde(default)]
    pub label: String,
    /// May be `null`, which is how a setting says "leave it to the game" —
    /// and which drops its flag from the launch line entirely. See
    /// [`super::template`].
    ///
    /// A string default may itself contain `{serverName}` / `{serverDir}`.
    #[serde(default)]
    pub default: Value,
    /// Inclusive bounds. `int` only.
    #[serde(default)]
    pub min: Option<i64>,
    #[serde(default)]
    pub max: Option<i64>,
    /// A closed set of legal values, when there is one. `string` only.
    #[serde(default)]
    pub options: Vec<Value>,
}

/// What a machine must have before it is offered this game.
///
/// Checked at `doctor` and again at create time, rather than the pipeline
/// avoiding heavy games. A game that only runs on a strong PC is in scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Requires {
    #[serde(default)]
    pub ram_mb: u64,
    #[serde(default)]
    pub disk_mb: u64,
    #[serde(default)]
    pub cpu_cores: Option<u32>,
}

/// The per-platform half: where the server comes from and how it is started.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Platform {
    #[serde(default)]
    pub runtime: Runtime,
    #[serde(default)]
    pub launch: Launch,
    /// Further pieces of the runtime, each fetched into its own folder inside
    /// the runtime directory.
    ///
    /// For a server that needs something its vendor does not ship with it: a
    /// Java server needs a Java runtime, and the vendor's download is only the
    /// game. Each component is pinned and verified exactly like `runtime`,
    /// lives at `<runtime dir>/<name>`, and is fetched before it, so `exe` may
    /// name a program inside one (`"jre/bin/java"`). Empty for every game
    /// whose download is the whole server.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<Component>,
}

/// One further piece of a platform's runtime. See [`Platform::components`].
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Component {
    /// Its folder inside the runtime directory: lowercase letters, digits and
    /// dashes, so it is the same path on every platform and cannot climb out.
    pub name: String,
    #[serde(default)]
    pub runtime: Runtime,
}

/// Where a game's server comes from.
///
/// Flattened rather than a tagged enum of structs so that an unknown `source`
/// degrades: see [`RuntimeSource`]. The fields each source needs are
/// validated in [`super::validate`], which can say *which* field a
/// steamcmd runtime is missing instead of serde saying the variant did not
/// match.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Runtime {
    #[serde(default)]
    pub source: RuntimeSource,

    // --- direct ---
    /// Vendor URL. **Ours is never a mirror**: we do not redistribute a game.
    #[serde(default)]
    pub url: Option<String>,
    /// Lowercase hex, 64 characters. Not optional for a direct source — an
    /// unpinned download is an executable we cannot vouch for.
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub extract: Option<Extract>,

    // --- steamcmd ---
    #[serde(default)]
    pub app_id: Option<u32>,
    /// Pins the build. `None` means "whatever is current", which is honest
    /// for a game that force-updates and refuses older clients.
    #[serde(default)]
    pub build_id: Option<String>,
    /// Rough install size, for the download UX and the disk check.
    #[serde(default)]
    pub size_mb: Option<u64>,
}

/// The kinds of runtime source this build can fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeSource {
    /// A URL, pinned by sha256.
    #[default]
    Direct,
    /// Valve's `steamcmd`, **anonymous login only**. A game that needs an
    /// account that owns it is out of scope, not a feature request.
    Steamcmd,
    /// A source added after this build shipped.
    ///
    /// This variant is why [`Runtime`] is not a tagged enum: an unknown
    /// `source` has to survive parsing so validation can refuse it in a
    /// sentence, rather than serde refusing the whole descriptor in a
    /// diagnostic.
    #[serde(other)]
    Unknown,
}

/// What to do with a downloaded file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Extract {
    /// Run it as it arrived.
    #[default]
    None,
    Zip,
}

/// The launch line, before templating.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Launch {
    /// Relative to the runtime directory. The host adds `.exe` and the exec
    /// bit — that is a platform question, not a descriptor one.
    #[serde(default)]
    pub exe: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Relative to whatever [`Launch::cwd_base`] names. `.` — the default —
    /// is that directory itself.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Which directory `cwd` is relative to.
    ///
    /// Defaults to the server directory, which is where saves have to land
    /// and what every descriptor written before this field existed meant.
    /// A game that resolves its own data relative to the working directory —
    /// such as the Rust pilot — needs `runtime` instead, and an absolute
    /// save-path argument or [`Saves::mounts`] to get its saves back out. See
    /// `docs/game-runner.md`.
    #[serde(default)]
    pub cwd_base: CwdBase,
}

/// Which directory a launch's working directory is relative to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CwdBase {
    /// The server directory: this server's own folder, backed up and moved
    /// as a unit.
    #[default]
    Server,
    /// The runtime directory: this game's installed files, shared by every
    /// server of that game on the machine.
    Runtime,
}

/// How a host knows the server is up.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ready {
    /// A substring of one console line. Not a regex: four console defects in
    /// one PowerNukkitX bring-up came from over-clever line parsing, and a
    /// substring is the shape a person can verify against a log by eye.
    #[serde(default)]
    pub marker: String,
    /// Generous on purpose. Rust generates a map on first boot; a timeout
    /// measured in seconds would fail every cold start.
    #[serde(default)]
    pub timeout_ms: u64,
}

/// How commands reach the server.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Console {
    #[serde(default)]
    pub via: ConsoleVia,
    #[serde(default)]
    pub rcon: Option<Rcon>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConsoleVia {
    /// Write a line to the process's stdin.
    #[default]
    Stdin,
    /// Speak RCON. Which dialect is [`Rcon::protocol`].
    Rcon,
    /// No console at all. A game with no console can still be stopped — by
    /// interrupt — but has no `console` command and no RCON player count.
    None,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rcon {
    #[serde(default)]
    pub protocol: RconProtocol,
    /// The **name of a declared port**, not a number — the runner may have
    /// had to move off the preferred port, and the console has to follow it.
    #[serde(default)]
    pub port: String,
    /// The name of a generated secret (`{secret:<name>}`), never a literal
    /// and never a setting: a password a player could read back out of the
    /// settings form is not a secret.
    #[serde(default)]
    pub secret: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RconProtocol {
    /// Valve's binary Source RCON over TCP.
    #[default]
    Source,
    /// Facepunch's JSON-over-WebSocket dialect.
    Webrcon,
}

/// How a server is asked to stop, and how long it is given.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stop {
    #[serde(default)]
    pub via: StopVia,
    /// The verb, for [`StopVia::Console`]. `stop` for a Minecraft server,
    /// `quit` for Rust.
    #[serde(default)]
    pub command: Option<String>,
    /// How long the polite rung is given before the next one. A save can take
    /// a while and a hard kill during one is how a world is lost.
    #[serde(default)]
    pub grace_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StopVia {
    /// Send [`Stop::command`] over whatever [`Console::via`] says.
    #[default]
    Console,
    /// A console control event. Unsupported on `win32` for a server we did
    /// not start in its own console group — which is why a game whose only
    /// stop is an interrupt is a platform gap, not a descriptor detail.
    Interrupt,
}

/// One port the server binds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Port {
    /// Referenced by `{port:<name>}` and by [`Rcon::port`].
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_protocol")]
    pub proto: Protocol,
    /// What the server prefers to bind, **and** the gateway's dest port. The
    /// public port is allocator-assigned and is never this number.
    #[serde(default)]
    pub port: u16,
    /// Carried through the gateway. `false` binds loopback only — which is
    /// where RCON belongs.
    #[serde(default)]
    pub expose: bool,
    /// Groups exposed ports into one gateway service. At most one TCP port
    /// per service; any number of UDP. `None` on an exposed port means a
    /// service of its own.
    #[serde(default)]
    pub service: Option<String>,
}

fn default_protocol() -> Protocol {
    Protocol::Udp
}

/// Written out rather than derived, because `tunnel::Protocol` has no
/// default and should not grow one: "a forward is UDP unless told" is this
/// module's claim about descriptors, not a claim about forwards in general.
/// It has to agree with [`default_protocol`], which is why it sits here.
impl Default for Port {
    fn default() -> Self {
        Self {
            name: String::new(),
            proto: default_protocol(),
            port: 0,
            expose: false,
            service: None,
        }
    }
}

/// A config file to write before starting.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigFile {
    /// Relative to the server directory.
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub format: ConfigFormat,
    /// Key in the file -> a templated value.
    #[serde(default)]
    pub keys: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigFormat {
    #[default]
    Properties,
    Ini,
    Json,
    Toml,
    Xml,
}

/// What to back up, and what is not worth carrying.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Saves {
    /// Relative to the server directory. A game whose saves cannot be put
    /// under it is out of scope — that is what makes "backup unit = server
    /// dir" hold.
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub excludes: Vec<String>,
    /// Directories a game insists on finding inside its runtime directory
    /// that must really live under the server directory.
    ///
    /// The escape hatch for the large class of servers that resolve both
    /// their game data and their saves against the working directory: data
    /// is shared per game, saves are per server, and one working directory
    /// cannot be both. Each entry is a link in the runtime directory
    /// pointing at a real directory under the server. See
    /// [`crate::engine::descriptor::Mount`] and `docs/game-runner.md`.
    #[serde(default)]
    pub mounts: Vec<Mount>,
}

/// One directory the game finds in its runtime that really lives under the
/// server.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mount {
    /// Where the game looks, relative to the runtime directory.
    #[serde(default)]
    pub runtime: String,
    /// Where the bytes are, relative to the server directory.
    #[serde(default)]
    pub server: String,
}

/// Where live facts about a running server come from.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Observe {
    #[serde(default)]
    pub players: PlayersVia,
    /// The console command that lists players, for [`PlayersVia::Rcon`].
    #[serde(default)]
    pub players_command: Option<String>,
    /// Console lines that mean someone joined or left.
    #[serde(default)]
    pub presence: Option<Presence>,
    #[serde(default)]
    pub ping: PingVia,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlayersVia {
    /// Valve's A2S query protocol, on a declared UDP port.
    A2s,
    /// Ask the console and parse the reply.
    Rcon,
    /// Count from [`Observe::presence`] lines.
    LogRegex,
    #[default]
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Presence {
    /// Substrings, for the same reason [`Ready::marker`] is one.
    #[serde(default)]
    pub join: String,
    #[serde(default)]
    pub leave: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PingVia {
    A2s,
    Tcp,
    #[default]
    None,
}

/// Whether this game supports mods, and eventually how.
///
/// Deliberately a per-game decision with no pipeline default: the shape of
/// the rest of this struct is decided by the first game that says yes.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mods {
    #[serde(default)]
    pub supported: bool,
}

impl GameDescriptor {
    /// Parse a descriptor. Shape errors only — see [`super::validate`] for
    /// whether it makes sense.
    pub fn parse(json: &str) -> crate::Result<Self> {
        serde_json::from_str(json).map_err(|err| {
            crate::Error::Malformed(format!(
                "this game's descriptor is not valid JSON: {err}. \
                 The file is part of the app, so this is a bug in Homerun \
                 rather than something you can fix."
            ))
        })
    }

    /// The platform entry for a host, if this game declares one.
    pub fn platform(&self, host: &str) -> Option<&Platform> {
        self.platforms.get(host)
    }

    /// Whether this game says it can run on a host at all.
    ///
    /// Declared, never inferred: a `platforms` entry without the matching
    /// `hosts` entry is a descriptor that has been half-edited, and
    /// validation rejects it rather than guessing which half was meant.
    pub fn runs_on(&self, host: &str) -> bool {
        self.hosts.iter().any(|h| h == host)
    }

    /// A declared port by name.
    pub fn port(&self, name: &str) -> Option<&Port> {
        self.ports.iter().find(|p| p.name == name)
    }

    /// A declared setting by key.
    pub fn setting(&self, key: &str) -> Option<&Setting> {
        self.settings.iter().find(|s| s.key == key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pilot's descriptor, near enough to the real one to be worth
    /// parsing in anger. Every value in it is a hypothesis for the probe —
    /// the *shape* is what these tests are about.
    pub(super) const RUST_JSON: &str = include_str!("testdata/rust.json");

    #[test]
    fn the_pilots_descriptor_parses() {
        let d = GameDescriptor::parse(RUST_JSON).expect("the pilot must parse");
        assert_eq!(d.id, "rust");
        assert_eq!(d.schema, SCHEMA_VERSION);
        assert!(d.runs_on("win32-x64"));
        assert_eq!(
            d.platform("win32-x64").unwrap().runtime.app_id,
            Some(258550)
        );
        assert_eq!(d.console.via, ConsoleVia::Rcon);
        assert_eq!(
            d.console.rcon.as_ref().unwrap().protocol,
            RconProtocol::Webrcon
        );
        assert_eq!(d.stop.via, StopVia::Console);
        assert_eq!(d.stop.command.as_deref(), Some("quit"));
        assert_eq!(d.port("game").unwrap().proto, Protocol::Udp);
        assert_eq!(d.port("rcon").unwrap().proto, Protocol::Tcp);
        assert!(!d.port("rcon").unwrap().expose);
    }

    /// The rule from the module header, and the reason it is a rule: a host
    /// in the field must run a game whose descriptor gained a field after
    /// that host shipped.
    #[test]
    fn a_descriptor_from_a_newer_schema_keeps_its_known_fields() {
        let d: GameDescriptor = serde_json::from_str(
            r#"{
                "id": "terraria",
                "name": "Terraria",
                "hosts": ["win32-x64"],
                "crossPlay": { "invented": true },
                "ports": [{ "name": "game", "proto": "tcp", "port": 7777, "expose": true,
                            "antiCheat": "none" }]
            }"#,
        )
        .expect("an unknown key must be ignored, not fatal");
        assert_eq!(d.id, "terraria");
        assert_eq!(d.port("game").unwrap().port, 7777);
    }

    /// The exception to that rule, and why it is spelled with `#[serde(other)]`
    /// rather than left to fail.
    #[test]
    fn an_unknown_runtime_source_parses_so_it_can_be_refused_in_words() {
        let r: Runtime = serde_json::from_str(r#"{ "source": "vendor-api" }"#)
            .expect("an unknown source must survive parsing");
        assert_eq!(r.source, RuntimeSource::Unknown);
    }

    #[test]
    fn a_malformed_descriptor_is_refused_in_a_sentence() {
        let err = GameDescriptor::parse("{ not json").unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("not valid JSON"),
            "must name the problem: {message}"
        );
        assert!(
            !message.contains("unwrap") && !message.contains("panicked"),
            "must read as a verdict, not a diagnostic: {message}"
        );
    }

    /// Absent is not the same as empty, and the difference is load-bearing:
    /// `licence: None` means "this game has no terms", which is a different
    /// claim from "these terms, unaccepted".
    #[test]
    fn an_absent_licence_stays_absent() {
        let d: GameDescriptor = serde_json::from_str(r#"{ "id": "x" }"#).unwrap();
        assert!(d.licence.is_none());
    }

    // ─── the wire format the API and the desktop read ───────────────────────
    //
    // Same reasoning as `game::tests`: a renamed field compiles cleanly and
    // fails at runtime in another repo, so each is pinned against literal
    // JSON.

    #[test]
    fn the_camel_case_names_are_the_contract() {
        let json = serde_json::to_value(GameDescriptor {
            id: "rust".into(),
            requires: Requires {
                ram_mb: 8192,
                disk_mb: 16000,
                cpu_cores: None,
            },
            ready: Ready {
                marker: "Server startup complete".into(),
                timeout_ms: 900_000,
            },
            stop: Stop {
                via: StopVia::Console,
                command: Some("quit".into()),
                grace_ms: 60_000,
            },
            client: Client {
                join_url: Some("steam://connect/{host}:{port:game}".into()),
                srv: None,
            },
            ..Default::default()
        })
        .unwrap();

        assert_eq!(json["requires"]["ramMb"], 8192);
        assert_eq!(json["requires"]["diskMb"], 16000);
        assert_eq!(json["ready"]["timeoutMs"], 900_000);
        assert_eq!(json["stop"]["graceMs"], 60_000);
        assert_eq!(
            json["client"]["joinUrl"],
            "steam://connect/{host}:{port:game}"
        );
    }

    #[test]
    fn the_enums_spell_themselves_as_the_descriptor_writes_them() {
        assert_eq!(
            serde_json::to_value(RuntimeSource::Steamcmd).unwrap(),
            "steamcmd"
        );
        assert_eq!(
            serde_json::to_value(RconProtocol::Webrcon).unwrap(),
            "webrcon"
        );
        assert_eq!(serde_json::to_value(ConsoleVia::Rcon).unwrap(), "rcon");
        assert_eq!(
            serde_json::to_value(StopVia::Interrupt).unwrap(),
            "interrupt"
        );
        assert_eq!(serde_json::to_value(Extract::Zip).unwrap(), "zip");
        assert_eq!(
            serde_json::to_value(ConfigFormat::Properties).unwrap(),
            "properties"
        );
        assert_eq!(serde_json::to_value(PingVia::A2s).unwrap(), "a2s");
        // kebab, because `log-regex` is how the plan and the contract write it
        assert_eq!(
            serde_json::to_value(PlayersVia::LogRegex).unwrap(),
            "log-regex"
        );
        assert_eq!(serde_json::to_value(SettingKind::Int).unwrap(), "int");
    }

    /// A port with no `proto` is UDP, not TCP. Stated because the wrong
    /// default is invisible: the server binds what it likes, the gateway
    /// forwards the other protocol, and the result is a server that runs and
    /// cannot be joined.
    #[test]
    fn a_port_without_a_protocol_is_udp() {
        let p: Port = serde_json::from_str(r#"{ "name": "game", "port": 28015 }"#).unwrap();
        assert_eq!(p.proto, Protocol::Udp);
    }

    #[test]
    fn a_descriptor_round_trips() {
        let original = GameDescriptor::parse(RUST_JSON).unwrap();
        let again: GameDescriptor =
            serde_json::from_str(&serde_json::to_string(&original).unwrap()).unwrap();
        assert_eq!(original, again);
    }
}
