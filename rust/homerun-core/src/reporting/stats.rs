//! What a running server looks like from outside: the periodic service-stats
//! report, the RCON replies it is built from, and when to send it.
//!
//! Reference: `nativeServerManager.ts` — `collectServiceStats`,
//! `reportServiceStats`, `scheduleStatsReport`, `maybeReportPlayerPresence`,
//! `parseListUuids`, `pinnedCommand`.
//!
//! # Why this is not two implementations
//!
//! This report is what the dashboard draws, what journeys count a session
//! from, and what support looks at when a player says nobody could join. All
//! of it is decided from two RCON replies and a clock, and none of that is
//! platform work — but written twice it is two regexes, two ideas of what
//! `memory_usage` is measured in, and two cadences.
//!
//! The units are the part that bites. `memory_usage` is **bytes** and every
//! platform reads KiB; `server_age` is **seconds** and the server answers in
//! ticks; `cpu_usage` is percent of the whole device while
//! [`crate::metrics`] deals in percent of one core. Each conversion happens
//! here, once, on the way into the payload.
//!
//! # The shape
//!
//! ```text
//!   host: RCON `list uuids`      →  parse_list_uuids  →  Roster
//!   host: RCON `time query …`    →  parse_server_age  →  seconds
//!   host: what only it knows     →  Stats { memory_kb, cpu_percent, … }
//!   core: report(service, device, &stats)  →  Request
//!   host: sign it, send it, forget it
//! ```
//!
//! The cadence is the same trade: [`Schedule`] is pure and holds no timer. The
//! host asks what is due and how long to sleep, and keeps the struct between
//! calls exactly as it keeps a [`crate::metrics::History`].
//!
//! # Parsing by hand
//!
//! The crate has no regex dependency and this module does not want one. Every
//! scan below is one left-to-right pass over the reply, so a garbled or
//! truncated line costs a scan and nothing else — the desktop's
//! `([^,:\s][^,:]*?)\s+\(…\)` over a long reply with no UUID in it backtracks
//! from every position instead.
//!
//! # Java only
//!
//! The desktop also reports Bedrock (BDS), which has no RCON: it builds a
//! roster from log lines and re-encodes each Xbox XUID as a Floodgate-style
//! UUID. Mobile hosts Java servers only in v1, so none of that is here — no
//! `xuid`, no Floodgate name-prefix stripping, no `There are N/M players
//! online` spelling (BDS's, and pre-1.13 Java's). A host that grows a Bedrock
//! engine adds it here rather than in Kotlin.
//!
//! # This module knows it is Minecraft
//!
//! [`crate::reporting`] otherwise sits in the game-agnostic layer, but a
//! roster of UUIDs and a `list uuids` reply are Minecraft's, and so is the
//! endpoint's schema. If a second game ever reports stats, the parsers move
//! under [`crate::minecraft`] and [`report`] stays where it is.

use crate::minecraft::jar::Loader;
use crate::reporting::Request;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Where the report goes. Trailing slash deliberate — see [`Request::path`].
pub const PATH: &str = "/api/reporting/service-stats/";

/// How often an idle server reports.
pub const REPORT_INTERVAL_MS: u64 = 120_000;

/// How long a join or leave waits before it turns into a report.
pub const PRESENCE_DEBOUNCE_MS: u64 = 1_000;

/// Minecraft's fixed tick rate. `time query gametime` answers in ticks.
pub const TICKS_PER_SECOND: u64 = 20;

/// The RCON command that answers with names *and* UUIDs in one round-trip.
/// Vanilla since 1.8.1. Pin it with [`pinned`].
pub const LIST_UUIDS: &str = "list uuids";

/// The RCON command the world's age comes from. Pin it with [`pinned`].
pub const GAMETIME: &str = "time query gametime";

// --- what a host must send -------------------------------------------------

/// Pin a vanilla command past any plugin that has shadowed it.
///
/// On Bukkit-family servers a plugin can take over a bare command name —
/// EssentialsX overrides both `/list` and `/time` and reformats their output,
/// which blanks `player_count` and `server_age` on exactly the servers people
/// run plugins on. `minecraft:` is Bukkit's command-map fallback to the
/// vanilla implementation.
///
/// It is only ever a *prefix*, never a fix for a missing command: vanilla,
/// Fabric, Quilt and (Neo)Forge are pure Brigadier, reject namespaced literals
/// outright, and have no Bukkit plugin to shadow anything anyway. So the
/// decision is per loader, and getting it backwards fails the command rather
/// than degrading.
pub fn pinned(command: &str, loader: Loader) -> String {
    match loader {
        // The desktop pins spigot and bukkit too; this crate cannot host them
        // (see `jar::Loader::parse`), and Paper is the same command map.
        Loader::Paper => format!("minecraft:{command}"),
        Loader::Vanilla => command.to_string(),
    }
}

/// One player the server says is online.
///
/// The UUID is the identity — a name is a label the player may change and a
/// plugin may decorate, so nothing downstream matches on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Player {
    pub name: String,
    /// Lowercase, hyphenated. Lowercased on the way in so one player is not
    /// two rows because an engine printed hex in caps.
    pub uuid: String,
}

/// Who is online, from one `list uuids` reply.
///
/// The count and the list travel together on purpose. The desktop parses them
/// separately, so a reply whose header it did not recognise reports
/// `player_count: null` **and** `players: []` — an empty roster, which reads
/// as "nobody is online" when the truth is "we could not tell". Here an
/// unrecognisable reply is [`None`] and both keys go null together.
///
/// A host whose engine exposes the roster directly — Pumpkin has no RCON —
/// builds one of these itself rather than round-tripping a string.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Roster {
    /// The server's own count, which is the authority. `None` when the reply
    /// carried a roster but not a header this understands.
    #[serde(default)]
    pub count: Option<u32>,
    /// The ceiling. Not part of the report — the UI's roster view needs it and
    /// it arrives in the same reply, so parsing it twice would be silly.
    #[serde(default)]
    pub max: Option<u32>,
    pub players: Vec<Player>,
}

/// Everything one report is built from.
///
/// Split by who can know it: the host fills the platform readings and the
/// clock-derived string, the parsers below fill the rest. Every field is
/// optional because every one of them has a way of being unavailable, and the
/// API distinguishes "unknown" from "zero".
///
/// **These field names are not the wire names.** [`report`] maps them onto the
/// endpoint's exact keys and applies the unit conversions; the names here say
/// what the *host* must supply, in the units a host actually reads.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    /// When this run started, ISO-8601 (`2026-08-12T09:41:07.000Z`).
    ///
    /// A string because this crate has no clock and must not grow one — it
    /// cannot read the time and it cannot format a timezone. The host holds
    /// the run's start instant anyway.
    #[serde(default)]
    pub server_runtime: Option<String>,
    /// Resident memory in KiB, as `/proc` and `task_info` report it. Sent as
    /// bytes.
    #[serde(default)]
    pub memory_kb: Option<u64>,
    /// CPU as a percentage **of the whole device**, 0–100, Task-Manager style.
    ///
    /// Not [`crate::metrics::Sample::cpu_percent`], which is percent of one
    /// core and deliberately exceeds 100 on a multi-core box. Feeding one
    /// straight into the other puts a phone's rows on a different scale from
    /// every desktop row in the same table — [`cpu_percent_of_device`] is the
    /// conversion.
    #[serde(default)]
    pub cpu_percent: Option<f64>,
    /// The parsed `list uuids` reply, or `None` when RCON did not answer.
    #[serde(default)]
    pub roster: Option<Roster>,
    /// Whether Mojang authenticated the players in `roster`. Decides whether
    /// anything downstream may trust those identities.
    #[serde(default)]
    pub online_mode: Option<bool>,
    /// The world's age in seconds, from [`parse_server_age`].
    #[serde(default)]
    pub server_age_secs: Option<f64>,
    /// The device's public IP, as the outside world sees it.
    #[serde(default)]
    pub host_ip: Option<String>,
    /// Round-trip to the gateway in milliseconds, or `None` when there is no
    /// gateway or it did not answer.
    #[serde(default)]
    pub gateway_ping_ms: Option<f64>,
}

/// Rescale a per-core CPU percentage onto the whole device.
///
/// [`crate::metrics`] measures a process against one core, so a server using
/// three cores reads 300. This endpoint's `cpu_usage` is the fraction of the
/// machine — 300 on a phone with 8 cores is 37.5. The two scales look
/// identical on a single-core reading, which is exactly why a mistake here
/// survives testing and only shows up as a phone that appears to be melting.
///
/// `None` when the core count is unknown, because a guess would be a wrong
/// number rather than a missing one.
pub fn cpu_percent_of_device(per_core_percent: f64, cores: u32) -> Option<f64> {
    if cores == 0 {
        return None;
    }
    Some(per_core_percent / cores as f64)
}

/// Build the report.
///
/// `service_id` is the API's id for this server, `device_id` this install's.
///
/// Every key the endpoint knows is present in the body even when its value is
/// null: the API stores a null as "we asked and could not tell", and a missing
/// key as "this client does not report that", which are different rows on a
/// graph.
///
/// The host is expected to check the server is *still running* immediately
/// before sending. Collecting the stats means two RCON round-trips, which can
/// span a stop, and a report that lands after one tells the dashboard a dead
/// server is alive.
pub fn report(service_id: &str, device_id: &str, stats: &Stats) -> Request {
    let players = stats.roster.as_ref().map(|roster| {
        roster
            .players
            .iter()
            .map(|player| {
                json!({
                    "name": player.name,
                    "uuid": player.uuid,
                    // Constant in v1 — mobile hosts Java only. See the module
                    // docs on why there is no `xuid` beside it.
                    "platform": "java",
                })
            })
            .collect::<Vec<Value>>()
    });

    Request::post(
        PATH,
        json!({
            "service": service_id,
            "device": device_id,
            "server_runtime": stats.server_runtime,
            // KiB in, bytes out. The desktop's `memUsedKb * 1024`.
            "memory_usage": stats.memory_kb.map(|kb| kb * 1024),
            "cpu_usage": stats.cpu_percent,
            "player_count": stats.roster.as_ref().and_then(|roster| roster.count),
            "players": players,
            "online_mode": stats.online_mode,
            "server_age": stats.server_age_secs,
            "host_ip_address": stats.host_ip,
            "gateway_ping": stats.gateway_ping_ms,
        }),
    )
}

// --- reading the server's replies ------------------------------------------

/// Parse a `list uuids` reply.
///
/// ```text
/// There are 2 of a max of 20 players online: Notch (069a79f4-…), Dinnerbone (2f21…)
/// ```
///
/// `None` when this is not a roster reply at all — a plugin-shadowed `/list`
/// ("Online Players (2/20): …"), an "Unknown or incomplete command", a
/// timeout's empty string. That is unknown, not empty; an empty server still
/// prints the header and yields a `Roster` with no players in it.
///
/// Entries whose parenthesised UUID is not a UUID are skipped: the UUID is
/// what makes a fragment a player, so a decorated or reformatted line loses
/// the entries it mangled rather than inventing them.
pub fn parse_list_uuids(reply: &str) -> Option<Roster> {
    // The header is the recognition test *and* the split point: names only
    // ever appear after it, so a MOTD or a plugin banner in the same reply
    // cannot contribute players.
    let (_, tail) = reply.split_once("players online")?;

    let (count, max) = match header_counts(reply) {
        Some((count, max)) => (Some(count), Some(max)),
        None => (None, None),
    };

    let players = tail.split(',').filter_map(parse_entry).collect();

    Some(Roster {
        count,
        max,
        players,
    })
}

/// `There are 2 of a max of 20 …` → `(2, 20)`.
fn header_counts(reply: &str) -> Option<(u32, u32)> {
    let (count, rest) = leading_number(reply.split_once("There are ")?.1)?;
    let (max, _) = leading_number(rest.strip_prefix(" of a max of ")?)?;
    Some((count as u32, max as u32))
}

/// One comma-separated `Name (uuid)` fragment.
fn parse_entry(entry: &str) -> Option<Player> {
    let open = entry.rfind('(')?;
    let uuid = entry[open + 1..].trim_end().strip_suffix(')')?;
    if !is_uuid(uuid) {
        return None;
    }

    // The last whitespace-separated token before the UUID, the way
    // `console::joined` reads a name — a Java name has no spaces in it, so
    // anything a loader or plugin put in front (`[VIP]`, the header's own
    // `:`) drops off. Java names cannot contain `:` either, so a prefix glued
    // straight on still comes apart.
    let name = entry[..open]
        .split_whitespace()
        .next_back()?
        .rsplit(':')
        .next()?
        .trim();
    if name.is_empty() {
        return None;
    }

    Some(Player {
        name: name.to_string(),
        uuid: uuid.to_ascii_lowercase(),
    })
}

/// 8-4-4-4-12 hex, and nothing else.
fn is_uuid(text: &str) -> bool {
    let mut groups = text.split('-');
    for length in [8, 4, 4, 4, 12] {
        match groups.next() {
            Some(group)
                if group.len() == length && group.bytes().all(|b| b.is_ascii_hexdigit()) => {}
            _ => return false,
        }
    }
    groups.next().is_none()
}

/// Parse a `time query gametime` reply into the world's age in seconds.
///
/// Two spellings, because Minecraft changed it:
///
/// ```text
/// The time is 1234567               // 1.21.4 and earlier
/// The game time is 1234567 tick(s)  // seen on 26.2
/// ```
///
/// The newer wording was found on a live server that was reporting a healthy
/// player count and a null `server_age`. The desktop knows only the older
/// spelling, so its reports have been quietly missing this field on current
/// versions as well.
///
/// Ticks, at twenty a second. This is the *world's* age and not this session's
/// — gametime does not advance while the server is stopped, so it accumulates
/// across runs, which is what the dashboard's "server age" means.
///
/// `None` when the reply is neither, which on a Bukkit-family server is the
/// ordinary consequence of an unpinned command: EssentialsX shadows `/time` as
/// well as `/list`, and that nulled `server_age` on precisely the servers
/// `/list` was already broken on.
pub fn parse_server_age(reply: &str) -> Option<f64> {
    // The newer phrase is tried first because the older one is not a substring
    // of it ("The time is" vs "The game time is"), so a reply carrying the new
    // wording would otherwise fall through to a match that cannot happen.
    let rest = reply
        .split_once("The game time is ")
        .or_else(|| reply.split_once("The time is "))?
        .1;
    let (ticks, _) = leading_number(rest)?;
    Some(ticks as f64 / TICKS_PER_SECOND as f64)
}

/// The unsigned decimal this text starts with, and what follows it.
///
/// One pass, no allocation, and `None` rather than a wrapped value when the
/// digits do not fit — a server that printed something absurd should read as
/// unknown.
fn leading_number(text: &str) -> Option<(u64, &str)> {
    let end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    Some((text[..end].parse().ok()?, &text[end..]))
}

// --- when to send ----------------------------------------------------------

/// Why a report is due.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Trigger {
    /// The interval elapsed, or the server has just started.
    Periodic,
    /// Somebody joined or left, and the coalescing window has closed. Worth
    /// distinguishing: the desktop also nudges any open Insights tab and takes
    /// an extra perf sample on these, so the player-count graph steps at the
    /// join rather than at the next tick.
    Presence,
}

/// How often to report, and how long to coalesce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cadence {
    pub interval_ms: u64,
    pub presence_debounce_ms: u64,
}

impl Default for Cadence {
    /// The desktop's numbers, so a phone-hosted server and a PC-hosted one
    /// produce the same density of rows.
    fn default() -> Self {
        Cadence {
            interval_ms: REPORT_INTERVAL_MS,
            presence_debounce_ms: PRESENCE_DEBOUNCE_MS,
        }
    }
}

/// One running server's report clock.
///
/// Pure and timer-free: the host drives it with its own `now_ms` and holds it
/// between calls, serialised, like [`crate::metrics::History`]. Nothing here
/// wakes anything up — [`wait_ms`](Self::wait_ms) says how long to sleep and
/// [`poll`](Self::poll) says what to send.
///
/// One per **run**. A restart starts a new one, so the launch report fires
/// again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    cadence: Cadence,
    /// When the periodic report comes due.
    due_at_ms: i64,
    /// When a coalescing presence report fires, if one is pending.
    #[serde(default)]
    presence_at_ms: Option<i64>,
}

impl Schedule {
    /// A server has just gone running.
    ///
    /// The first report is due **immediately**, not one interval from now: the
    /// dashboard should have real data — a gateway ping above all — the moment
    /// a player is told the server is up, rather than showing an empty card
    /// for two minutes.
    pub fn started(cadence: Cadence, now_ms: i64) -> Self {
        Schedule {
            cadence,
            due_at_ms: now_ms,
            presence_at_ms: None,
        }
    }

    /// Somebody joined or left.
    ///
    /// Arms a report one debounce from now, and **re-arms** it if one was
    /// already pending, so a party landing together produces one report
    /// instead of six. The delay also gives the server a moment to settle its
    /// player list — asking `list uuids` on the same tick as the join line can
    /// answer without the player who caused it.
    ///
    /// Without any of this, a session shorter than the interval was invisible:
    /// no report ever caught the player online, so journeys never saw them.
    pub fn presence(&mut self, now_ms: i64) {
        self.presence_at_ms = Some(now_ms + self.cadence.presence_debounce_ms as i64);
    }

    /// What to send now, if anything. Consumes what it returns.
    ///
    /// A presence report **resets the periodic clock**, so the interval always
    /// measures from the last report actually sent and a tick never lands a
    /// second after an event-driven one.
    ///
    /// A periodic report that comes due while a presence report is still
    /// coalescing goes out on its own schedule and does not cancel it — the
    /// join is what the second report exists to capture, and dropping it would
    /// lose the roster change this whole path is for.
    pub fn poll(&mut self, now_ms: i64) -> Option<Trigger> {
        if let Some(at) = self.presence_at_ms {
            if now_ms >= at {
                self.presence_at_ms = None;
                self.due_at_ms = now_ms + self.cadence.interval_ms as i64;
                return Some(Trigger::Presence);
            }
        }

        if now_ms >= self.due_at_ms {
            self.due_at_ms = now_ms + self.cadence.interval_ms as i64;
            return Some(Trigger::Periodic);
        }

        None
    }

    /// When [`poll`](Self::poll) next has something to say, as wall clock.
    ///
    /// For hosts that schedule against an absolute time — Android's
    /// `AlarmManager` — rather than a delay.
    pub fn next_at_ms(&self) -> i64 {
        match self.presence_at_ms {
            Some(at) => at.min(self.due_at_ms),
            None => self.due_at_ms,
        }
    }

    /// How long to sleep before polling again. Zero when something is already
    /// due.
    pub fn wait_ms(&self, now_ms: i64) -> u64 {
        self.next_at_ms().saturating_sub(now_ms).max(0) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporting::{Auth, Method};

    // Replies as a server prints them: RCON returns the command's output with
    // no log prefix, so these are whole strings.
    const TWO_ONLINE: &str = "There are 2 of a max of 20 players online: Notch \
                              (069a79f4-44e9-4726-a5be-fca90e38aaf5), Dinnerbone \
                              (2f5d0e29-6ec1-4b0b-8b4e-6a3e4e1b4f3d)";
    const NOBODY_ONLINE: &str = "There are 0 of a max of 20 players online:";

    // --- the roster --------------------------------------------------------

    #[test]
    fn a_reply_names_everyone_online_and_the_servers_own_count() {
        let roster = parse_list_uuids(TWO_ONLINE).expect("a list reply");
        assert_eq!(roster.count, Some(2));
        assert_eq!(roster.max, Some(20));
        assert_eq!(
            roster.players,
            vec![
                Player {
                    name: "Notch".into(),
                    uuid: "069a79f4-44e9-4726-a5be-fca90e38aaf5".into(),
                },
                Player {
                    name: "Dinnerbone".into(),
                    uuid: "2f5d0e29-6ec1-4b0b-8b4e-6a3e4e1b4f3d".into(),
                },
            ]
        );
    }

    #[test]
    fn an_empty_server_is_nobody_online_rather_than_unknown() {
        let roster = parse_list_uuids(NOBODY_ONLINE).expect("a list reply");
        assert_eq!(roster.count, Some(0));
        assert_eq!(roster.max, Some(20));
        assert!(
            roster.players.is_empty(),
            "invented players out of an empty roster: {:?}",
            roster.players
        );
    }

    /// The distinction the desktop loses: a reply it cannot read still becomes
    /// an empty player list there, which the API cannot tell from an idle
    /// server.
    #[test]
    fn a_reply_that_is_not_a_roster_is_unknown_not_empty() {
        // EssentialsX's /list, which is why the command is pinned at all.
        assert_eq!(
            parse_list_uuids("Online Players (2/20): Notch, Dinnerbone"),
            None
        );
        // A namespaced command a Brigadier server rejected.
        assert_eq!(
            parse_list_uuids("Unknown or incomplete command, see below for error"),
            None
        );
        // A timeout, as far as a host is concerned.
        assert_eq!(parse_list_uuids(""), None);
    }

    #[test]
    fn an_entry_without_a_real_uuid_is_not_a_player() {
        let roster =
            parse_list_uuids("There are 1 of a max of 20 players online: Notch (not-a-uuid)")
                .expect("a list reply");
        assert!(
            roster.players.is_empty(),
            "reported a player whose identity was junk: {:?}",
            roster.players
        );
        // The server's own count still stands — it said one player is on, and
        // only the identity was unreadable.
        assert_eq!(roster.count, Some(1));
    }

    #[test]
    fn a_uuid_is_read_however_the_engine_spelled_it() {
        let roster = parse_list_uuids(
            "There are 1 of a max of 20 players online: Notch (069A79F4-44E9-4726-A5BE-FCA90E38AAF5)",
        )
        .expect("a list reply");
        assert_eq!(
            roster.players[0].uuid, "069a79f4-44e9-4726-a5be-fca90e38aaf5",
            "the same player would be two rows in the API"
        );
    }

    /// A UUID is 8-4-4-4-12 and nothing adjacent to it counts.
    #[test]
    fn near_misses_are_not_uuids() {
        assert!(is_uuid("069a79f4-44e9-4726-a5be-fca90e38aaf5"));
        // A digit short in the last group.
        assert!(!is_uuid("069a79f4-44e9-4726-a5be-fca90e38aaf"));
        // A group too many.
        assert!(!is_uuid("069a79f4-44e9-4726-a5be-fca90e38aaf5-0000"));
        // Not hex.
        assert!(!is_uuid("069a79f4-44e9-4726-a5be-fca90e38aazz"));
        assert!(!is_uuid(""));
    }

    // --- the world's age ---------------------------------------------------

    #[test]
    fn gametime_reads_as_seconds_not_ticks() {
        // A day and a bit of gametime.
        assert_eq!(parse_server_age("The time is 1234567"), Some(61_728.35));
        assert_eq!(parse_server_age("The time is 0"), Some(0.0));
    }

    /// Minecraft reworded this. Both lines below were copied from real server
    /// logs on the same device — 1.21.4 and 26.2 — after a live server
    /// reported a healthy player count beside a null age.
    #[test]
    fn both_spellings_of_the_gametime_reply_are_understood() {
        assert_eq!(
            parse_server_age("[20:37:28] [Server thread/INFO]: The game time is 47632 tick(s)"),
            Some(2_381.6)
        );
        assert_eq!(
            parse_server_age("[20:23:05] [Server thread/INFO]: The time is 32082"),
            Some(1_604.1)
        );
    }

    #[test]
    fn a_shadowed_time_command_leaves_the_age_unknown() {
        // EssentialsX's /time, reformatted.
        assert_eq!(parse_server_age("It is currently day 5, 06:00"), None);
        assert_eq!(parse_server_age("The time is "), None);
        assert_eq!(parse_server_age(""), None);
    }

    // --- pinning -----------------------------------------------------------

    #[test]
    fn a_plugin_shadowed_command_is_pinned_only_where_a_plugin_could_shadow_it() {
        assert_eq!(pinned(LIST_UUIDS, Loader::Paper), "minecraft:list uuids");
        assert_eq!(
            pinned(GAMETIME, Loader::Paper),
            "minecraft:time query gametime"
        );
        // Brigadier rejects the namespaced literal outright, so pinning here
        // would break the command it was meant to protect.
        assert_eq!(pinned(LIST_UUIDS, Loader::Vanilla), "list uuids");
        assert_eq!(pinned(GAMETIME, Loader::Vanilla), "time query gametime");
    }

    // --- the payload -------------------------------------------------------

    fn full_stats() -> Stats {
        Stats {
            server_runtime: Some("2026-08-12T09:41:07.000Z".into()),
            memory_kb: Some(2048),
            cpu_percent: Some(12.5),
            roster: parse_list_uuids(TWO_ONLINE),
            online_mode: Some(true),
            // A literal, not `parse_server_age` — the conversion is that
            // function's rule, and a payload test that restated it would fail
            // for two unrelated reasons.
            server_age_secs: Some(61_728.35),
            host_ip: Some("203.0.113.7".into()),
            gateway_ping_ms: Some(42.0),
        }
    }

    #[test]
    fn the_report_goes_where_the_api_is_listening() {
        let request = report("srv-1", "dev-9", &full_stats());
        assert_eq!(request.method, Method::Post);
        assert_eq!(request.path, "/api/reporting/service-stats/");
        assert!(
            request.path.ends_with('/'),
            "Django redirects a slashless POST into a GET and loses the body"
        );
        // A report is a fact about a machine, not an act by a person.
        assert_eq!(request.auth, Auth::Device);
        assert_eq!(request.body["service"], json!("srv-1"));
        assert_eq!(request.body["device"], json!("dev-9"));
    }

    #[test]
    fn every_reading_reaches_its_key_in_the_units_the_api_stores() {
        let body = report("srv-1", "dev-9", &full_stats()).body;
        assert_eq!(
            body["memory_usage"],
            json!(2_097_152),
            "2 MiB reported as {} — the dashboard would read KiB as bytes",
            body["memory_usage"]
        );
        assert_eq!(body["server_age"], json!(61_728.35));
        assert_eq!(body["cpu_usage"], json!(12.5));
        assert_eq!(body["online_mode"], json!(true));
        assert_eq!(body["host_ip_address"], json!("203.0.113.7"));
        assert_eq!(body["gateway_ping"], json!(42.0));
        assert_eq!(body["server_runtime"], json!("2026-08-12T09:41:07.000Z"));
    }

    #[test]
    fn the_roster_fills_the_count_and_the_list_from_one_reply() {
        let body = report("srv-1", "dev-9", &full_stats()).body;
        assert_eq!(body["player_count"], json!(2));
        assert_eq!(
            body["players"],
            json!([
                {
                    "name": "Notch",
                    "uuid": "069a79f4-44e9-4726-a5be-fca90e38aaf5",
                    "platform": "java",
                },
                {
                    "name": "Dinnerbone",
                    "uuid": "2f5d0e29-6ec1-4b0b-8b4e-6a3e4e1b4f3d",
                    "platform": "java",
                },
            ])
        );
    }

    /// A key that is absent reads as "this client does not report that" and a
    /// key that is null as "we asked and could not tell". The API stores them
    /// differently, so nothing may be dropped for being unknown.
    #[test]
    fn every_key_survives_being_unknown() {
        let body = report("srv-1", "dev-9", &Stats::default()).body;
        for key in [
            "server_runtime",
            "memory_usage",
            "cpu_usage",
            "player_count",
            "players",
            "online_mode",
            "server_age",
            "host_ip_address",
            "gateway_ping",
        ] {
            assert_eq!(
                body.get(key),
                Some(&Value::Null),
                "`{key}` went missing instead of going null: {body}"
            );
        }
    }

    #[test]
    fn an_unreadable_roster_nulls_the_count_and_the_list_together() {
        let stats = Stats {
            roster: parse_list_uuids("Online Players (2/20): Notch"),
            ..Stats::default()
        };
        let body = report("srv-1", "dev-9", &stats).body;
        assert_eq!(body["player_count"], Value::Null);
        assert_eq!(
            body["players"],
            Value::Null,
            "an unreadable reply was reported as an empty server"
        );
    }

    #[test]
    fn an_idle_server_reports_an_empty_roster_rather_than_nothing() {
        let stats = Stats {
            roster: parse_list_uuids(NOBODY_ONLINE),
            ..Stats::default()
        };
        let body = report("srv-1", "dev-9", &stats).body;
        assert_eq!(body["player_count"], json!(0));
        assert_eq!(body["players"], json!([]));
    }

    #[test]
    fn a_per_core_percentage_is_rescaled_before_it_is_reported() {
        // Three cores busy on an eight-core phone.
        assert_eq!(cpu_percent_of_device(300.0, 8), Some(37.5));
        // One core busy on a single-core device: the scales coincide, which is
        // why the mistake survives a naive test.
        assert_eq!(cpu_percent_of_device(100.0, 1), Some(100.0));
        assert_eq!(cpu_percent_of_device(100.0, 0), None);
    }

    // --- the cadence -------------------------------------------------------

    fn schedule() -> Schedule {
        Schedule::started(Cadence::default(), 0)
    }

    #[test]
    fn a_launch_reports_at_once_rather_than_two_minutes_later() {
        let mut schedule = schedule();
        assert_eq!(schedule.wait_ms(0), 0);
        assert_eq!(schedule.poll(0), Some(Trigger::Periodic));
    }

    #[test]
    fn nothing_is_due_before_the_interval_elapses() {
        let mut schedule = schedule();
        schedule.poll(0);

        assert_eq!(schedule.poll(1), None);
        assert_eq!(schedule.poll(119_999), None);
        assert_eq!(schedule.wait_ms(119_999), 1);
        assert_eq!(schedule.poll(120_000), Some(Trigger::Periodic));
        // And it re-arms itself for the next one.
        assert_eq!(schedule.poll(120_001), None);
        assert_eq!(schedule.poll(240_000), Some(Trigger::Periodic));
    }

    /// A party landing together fires a join line each. One report.
    #[test]
    fn a_burst_of_joins_produces_one_report() {
        let mut schedule = schedule();
        schedule.poll(0);

        let mut reports = Vec::new();
        // Four players over 400 ms, and a host polling all the way through.
        for at in [1_000, 1_100, 1_200, 1_400] {
            schedule.presence(at);
            if let Some(trigger) = schedule.poll(at) {
                reports.push((at, trigger));
            }
        }
        assert!(
            reports.is_empty(),
            "a report went out mid-burst, so each of four joins gets its own: {reports:?}"
        );

        // The window closes one debounce after the *last* join, not the first.
        assert_eq!(schedule.poll(2_000), None);
        assert_eq!(schedule.poll(2_400), Some(Trigger::Presence));
        assert_eq!(schedule.poll(2_401), None);
    }

    /// The rule that keeps a tick from landing a second after an event-driven
    /// report: the interval measures from what was actually sent.
    #[test]
    fn a_presence_report_pushes_the_periodic_one_out() {
        let mut schedule = schedule();
        schedule.poll(0);

        // A join just before the periodic report would have gone out.
        schedule.presence(119_000);
        assert_eq!(schedule.poll(120_000), Some(Trigger::Presence));

        assert_eq!(
            schedule.poll(120_001),
            None,
            "the periodic report fired a millisecond after the presence one"
        );
        assert_eq!(schedule.wait_ms(120_000), 120_000);
        assert_eq!(schedule.poll(240_000), Some(Trigger::Periodic));
    }

    /// The converse: a periodic report coming due mid-debounce does not eat
    /// the presence one. The join is the thing that report exists to carry.
    #[test]
    fn a_periodic_tick_does_not_swallow_a_pending_join() {
        let mut schedule = schedule();
        schedule.poll(0);

        schedule.presence(119_900);
        assert_eq!(schedule.poll(120_000), Some(Trigger::Periodic));
        assert_eq!(
            schedule.poll(120_900),
            Some(Trigger::Presence),
            "the join never reached the API"
        );
    }

    #[test]
    fn a_host_is_told_how_long_to_sleep() {
        let mut schedule = schedule();
        schedule.poll(0);
        assert_eq!(schedule.wait_ms(0), 120_000);
        assert_eq!(schedule.next_at_ms(), 120_000);

        // A pending join is always the nearer of the two.
        schedule.presence(500);
        assert_eq!(schedule.wait_ms(500), 1_000);
        assert_eq!(schedule.next_at_ms(), 1_500);

        // Nothing goes negative once something is overdue.
        assert_eq!(schedule.wait_ms(9_000), 0);
    }

    #[test]
    fn a_schedule_survives_the_round_trip_a_host_puts_it_through() {
        let mut schedule = schedule();
        schedule.poll(0);
        schedule.presence(1_000);

        let wire = serde_json::to_string(&schedule).unwrap();
        let mut back: Schedule = serde_json::from_str(&wire).unwrap();
        assert_eq!(back, schedule);

        // Including the join that had not fired yet when it was stored.
        assert_eq!(back.poll(1_500), None);
        assert_eq!(back.poll(2_000), Some(Trigger::Presence));
    }

    #[test]
    fn stats_survive_the_round_trip_a_host_puts_them_through() {
        let stats = full_stats();
        let wire = serde_json::to_string(&stats).unwrap();
        let back: Stats = serde_json::from_str(&wire).unwrap();
        assert_eq!(back, stats);

        // And a host that fills in only what it has still deserialises.
        let sparse: Stats = serde_json::from_str(r#"{"memoryKb":1024}"#).unwrap();
        assert_eq!(sparse.memory_kb, Some(1024));
        assert_eq!(sparse.roster, None);
    }
}
