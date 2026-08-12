//! Keeping an operator granted from the console from quietly losing op at the
//! next launch.
//!
//! Reference: `nativeServerManager.ts` — `syncOpsEnvFromCommand:2073` and
//! `sendRcon:2043`, plus `parseEnvUserList` in `serverEnv.ts`.
//!
//! # The bug this closes
//!
//! `ops.json` is rewritten wholesale from the API's `OPS` environment variable
//! on every launch ([`super::settings::ops_json`]), and wholesale is
//! deliberate: it is the only reason a player *removed* from the list stops
//! being an operator. The cost is the other direction. An operator granted only
//! by typing `/op Name` into the console exists until the next start and then
//! silently does not, with nothing in any log to say why. `BANNED` is the same
//! story in a different shape — it merges append-only into
//! `banned-players.json` ([`super::settings::merge_banned`]), so a ban that
//! never reaches the API stays on whichever device happened to be hosting that
//! day and the griefer walks back in from the next one.
//!
//! So every console `op`/`deop`/`ban`/`pardon` has to be mirrored back into the
//! server's environment. Which commands count, which key holds the list,
//! whether this particular command changes anything, and what the resulting
//! request looks like are the same questions on every platform — they are here.
//! The GET and the PATCH are the host's.
//!
//! ```text
//!   host: a console command   →  ops::parse   → Some(Command)
//!   host: GET /api/server/<id>/
//!   host: the body            →  ops::sync    → Some(Change)
//!   host: PATCH it, then echo Change::line to the player
//! ```
//!
//! # Which credential signs it, and why that is not a detail
//!
//! [`Auth::User`], never [`Auth::Device`] — the one place in this crate that
//! asks for the person rather than the machine. The API's role engine judges a
//! settings PATCH against whoever signed it, and a member who may not change
//! ops in the UI has that same change **silently stripped** here: the request
//! succeeds, the response looks fine, and the environment is unchanged. A host
//! that reaches for the device token because it is the one always to hand gets
//! exactly that failure, plus a console line telling the player their op was
//! saved. Sign it as the person who issued the command — the caller on the
//! device websocket, or the signed-in session.
//!
//! If the host has no user credential at all, it must skip the sync entirely
//! rather than substituting the device token. The command still runs; it just
//! does not outlive the session, which is the behaviour that existed before any
//! of this.
//!
//! # Java only
//!
//! The desktop skips this whole path for Bedrock: BDS permissions do not come
//! from `ops.json` and its `OPS` entries are `gamertag:xuid` pairs, so reading
//! a bare name out of a console line is wrong there twice over. Mobile hosts
//! Java servers only, so there is no game flag in this module. If a Bedrock
//! engine ever lands, gate this at the call site — do not teach the parser
//! about a second format.
//!
//! # One at a time, per server
//!
//! This is a read-modify-write across two round trips, and the caller **must**
//! serialise it per server. The desktop chains the syncs for exactly that
//! reason: `/op A` and `/op B` typed a moment apart both read the pre-`A` list,
//! the second PATCH lands last and erases `A`, and the console said "saved"
//! twice.
//!
//! Nothing here fails loudly. A sync that does not happen costs persistence
//! across restarts, and that is never worth interrupting a running server for.

use crate::minecraft::settings::parse_user_list;
use crate::reporting::{Auth, Method, Request};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// Which of the server's two player lists a command touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum List {
    Ops,
    Banned,
}

impl List {
    /// The canonical environment key: a comma-separated string.
    pub fn key(self) -> &'static str {
        match self {
            List::Ops => "OPS",
            List::Banned => "BANNED",
        }
    }

    /// The legacy array key, read only when the canonical one is absent. See
    /// [`parse_user_list`] for why key presence rather than truthiness decides.
    pub fn legacy_key(self) -> &'static str {
        match self {
            List::Ops => "op_users",
            List::Banned => "banned_users",
        }
    }
}

/// The four console commands worth mirroring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Verb {
    Op,
    Deop,
    Ban,
    Pardon,
}

impl Verb {
    fn from_word(word: &str) -> Option<Verb> {
        // Case-insensitive but whole-word: the desktop's `/…/i` still anchors,
        // so `oped Name` is not an `op`.
        [
            ("op", Verb::Op),
            ("deop", Verb::Deop),
            ("ban", Verb::Ban),
            ("pardon", Verb::Pardon),
        ]
        .into_iter()
        .find(|(name, _)| word.eq_ignore_ascii_case(name))
        .map(|(_, verb)| verb)
    }

    pub fn list(self) -> List {
        match self {
            Verb::Op | Verb::Deop => List::Ops,
            Verb::Ban | Verb::Pardon => List::Banned,
        }
    }

    /// Whether this command puts the name **on** its list.
    pub fn adding(self) -> bool {
        matches!(self, Verb::Op | Verb::Ban)
    }
}

/// A console line that asks for a list change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Command {
    pub verb: Verb,
    /// As typed. The list is matched case-insensitively but stored verbatim, so
    /// the API keeps the capitalisation the operator used.
    pub name: String,
}

/// What the host should do about a command that actually changes something.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub list: List,
    /// The list after the change, in the order it will be stored.
    pub users: Vec<String>,
    pub request: Request,
    /// What to echo into the console — **after** the PATCH succeeds. It says
    /// the change was saved, so sending it first makes it a lie in the one case
    /// where the player needed the truth.
    pub line: String,
}

/// Whether a console line is one of the four commands, and on whom.
///
/// Mirrors the desktop's two regexes, and there is no regex crate here to hide
/// behind:
///
/// ```text
///   /^\/?(op|deop|pardon)\s+(\w{1,16})$/i
///   /^\/?(ban)\s+(\w{1,16})(?:\s+.+)?$/i
/// ```
///
/// So: the leading `/` is optional, the verb is case-insensitive, exactly one
/// name follows it, and that name is 1–16 of `[A-Za-z0-9_]` — Minecraft's own
/// charset, which is why a name with a dot or a seventeenth character is not a
/// name and not our business. Only `ban` accepts anything after the name, and
/// takes all of it as the reason.
///
/// `None` means "not a list change", which covers every other console command
/// and is the overwhelmingly common answer. The host does no GET for it.
pub fn parse(command: &str) -> Option<Command> {
    let text = command.trim();
    let text = text.strip_prefix('/').unwrap_or(text);

    // `\s+` between the verb and the name: the split point must be whitespace,
    // so a bare `op` with no argument has nowhere to split and does not match.
    let split = text.find(char::is_whitespace)?;
    let verb = Verb::from_word(&text[..split])?;
    let after = text[split..].trim_start();

    // `\w{1,16}`: JavaScript's `\w` without the unicode flag is ASCII only, and
    // so is a Minecraft name.
    let name: String = after
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() || name.len() > 16 {
        return None;
    }

    let tail = &after[name.len()..];
    let accepted = if tail.is_empty() {
        true
    } else if verb == Verb::Ban {
        // `(?:\s+.+)?$` — whitespace, then a reason. `.` does not match a line
        // terminator in JavaScript, and a console line carrying one is not a
        // command anybody typed.
        let reason = tail.trim_start();
        reason.len() != tail.len()
            && !reason.is_empty()
            && !reason.contains(['\n', '\r', '\u{2028}', '\u{2029}'])
    } else {
        // `$` right after the name. `op Notch please` is not an op.
        false
    };

    accepted.then_some(Command { verb, name })
}

/// The request and console line for a command, given the server the API
/// returned.
///
/// `server` is the whole `GET /api/server/<id>/` body; the environment lives at
/// `config.environment_variables`, and anything missing along the way means an
/// empty environment rather than an error — a server whose config has not been
/// written yet genuinely has no operators.
///
/// `None` means the list already says what the command asks for, so there is
/// nothing to send. That is not a failure and the host should not report one:
/// the command itself still ran, so `/op` on someone who is already an operator
/// behaves exactly as the server does.
pub fn sync(command: &Command, server: &Value, server_id: &str) -> Option<Change> {
    let env = server
        .get("config")
        .and_then(|config| config.get("environment_variables"))
        .cloned()
        .unwrap_or(Value::Null);
    sync_env(command, &env, server_id)
}

/// [`sync`], for a host that already holds `environment_variables` itself.
pub fn sync_env(command: &Command, env: &Value, server_id: &str) -> Option<Change> {
    let list = command.verb.list();
    let current = parse_user_list(env, list.key(), &[list.legacy_key()]);

    // Case-insensitive, because Minecraft names are case-insensitively unique:
    // `/deop notch` must remove `Notch`, or the console reports success and the
    // next launch re-ops him from an entry that never went away.
    let present = current
        .iter()
        .any(|user| user.eq_ignore_ascii_case(&command.name));
    let adding = command.verb.adding();
    if adding == present {
        return None;
    }

    let users: Vec<String> = if adding {
        let mut next = current;
        next.push(command.name.clone());
        next
    } else {
        current
            .into_iter()
            .filter(|user| !user.eq_ignore_ascii_case(&command.name))
            .collect()
    };

    let mut environment = Map::new();
    environment.insert(list.key().to_string(), Value::String(users.join(",")));

    let joined = users.join(", ");
    let line = match list {
        List::Ops => format!("[Homerun] Operator change saved to server settings: ops=[{joined}]"),
        List::Banned => {
            format!("[Homerun] Ban change saved to server settings: banned=[{joined}]")
        }
    };

    Some(Change {
        list,
        users,
        request: Request {
            method: Method::Patch,
            path: format!("/api/server/{server_id}/"),
            body: json!({ "environment_variables": Value::Object(environment) }),
            // See the module doc: the device token gets this stripped in
            // silence.
            auth: Auth::User,
        },
        line,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(line: &str) -> Command {
        parse(line).unwrap_or_else(|| panic!("{line:?} should be a list command"))
    }

    fn env(pairs: &[(&str, Value)]) -> Value {
        let mut map = Map::new();
        for (key, value) in pairs {
            map.insert((*key).to_string(), value.clone());
        }
        Value::Object(map)
    }

    // ─── what counts as a command ───────────────────────────────────────────

    #[test]
    fn the_four_verbs_are_recognised_with_or_without_a_slash() {
        for (line, verb) in [
            ("op Notch", Verb::Op),
            ("/op Notch", Verb::Op),
            ("deop Notch", Verb::Deop),
            ("/DeOp Notch", Verb::Deop),
            ("ban Notch", Verb::Ban),
            ("/pardon Notch", Verb::Pardon),
        ] {
            let parsed = command(line);
            assert_eq!(parsed.verb, verb, "{line:?}");
            assert_eq!(parsed.name, "Notch", "{line:?}");
        }
    }

    /// Only `ban` takes a reason. `op Notch please` is not an op, and reading
    /// it as one would op somebody off a line the server itself rejected.
    #[test]
    fn only_ban_accepts_a_trailing_reason() {
        let parsed = command("ban Griefer a reason here");
        assert_eq!(parsed.verb, Verb::Ban);
        assert_eq!(parsed.name, "Griefer");

        for line in [
            "op Notch please",
            "deop Notch now",
            "pardon Notch sorry",
            "/op Notch 4",
        ] {
            assert_eq!(parse(line), None, "{line:?} must not be a list change");
        }
    }

    #[test]
    fn a_name_the_game_could_not_have_is_refused() {
        for line in [
            "op Not.ch",             // a dot is not a name character
            "ban Not.ch reason",     // and not with a reason either
            "op ",                   // nothing to op
            "op",                    // nothing at all
            "/op",                   //
            "/ op Notch",            // the slash is a prefix, not a word
            "oped Notch",            // the verb is a whole word
            "opNotch",               //
            "say op Notch",          // an op inside chat is chat
            "list",                  //
            "",                      //
            "op Notch Steve",        // one name, not two
            "op Notch\nban Someone", // no smuggling a second command
        ] {
            assert_eq!(parse(line), None, "{line:?} must not parse");
        }
    }

    #[test]
    fn a_sixteen_character_name_is_still_a_name() {
        // The longest name Mojang ever issued, and one character more.
        assert_eq!(command("op abcdefghijklmnop").name, "abcdefghijklmnop");
        assert_eq!(
            parse("op abcdefghijklmnopq"),
            None,
            "17 characters is not a name any Minecraft account has"
        );
    }

    #[test]
    fn surrounding_whitespace_does_not_change_the_command() {
        let parsed = command("  /op   Notch  ");
        assert_eq!(parsed.verb, Verb::Op);
        assert_eq!(parsed.name, "Notch");
    }

    // ─── the decision ───────────────────────────────────────────────────────

    #[test]
    fn opping_someone_new_patches_the_list() {
        let server = json!({ "config": { "environment_variables": { "OPS": "Steve" } } });
        let change = sync(&command("/op Notch"), &server, "srv-1").expect("a change");

        assert_eq!(change.list, List::Ops);
        assert_eq!(change.users, vec!["Steve", "Notch"]);
        assert_eq!(change.request.method, Method::Patch);
        assert_eq!(change.request.path, "/api/server/srv-1/");
        assert_eq!(
            change.request.body,
            json!({ "environment_variables": { "OPS": "Steve,Notch" } })
        );
        assert_eq!(
            change.line,
            "[Homerun] Operator change saved to server settings: ops=[Steve, Notch]"
        );
    }

    /// The one place in this crate that signs as the person. A member's
    /// settings PATCH is judged against whoever signed it; with the device
    /// token the API strips the change, answers 200, and the player is told
    /// their op was saved.
    #[test]
    fn the_patch_is_signed_by_the_person_not_the_device() {
        let change = sync_env(&command("op Notch"), &json!({}), "srv-1").expect("a change");
        assert_eq!(
            change.request.auth,
            Auth::User,
            "a device-signed settings PATCH is silently stripped"
        );
    }

    /// Minecraft names are case-insensitively unique. Matching exactly would
    /// append a second `notch` beside `Notch` and re-op him at the next launch.
    #[test]
    fn membership_ignores_case() {
        let env = env(&[("OPS", json!("Notch,Steve"))]);

        assert_eq!(
            sync_env(&command("op notch"), &env, "srv-1"),
            None,
            "already an operator under another capitalisation"
        );

        let removed = sync_env(&command("deop NOTCH"), &env, "srv-1").expect("a change");
        assert_eq!(
            removed.users,
            vec!["Steve"],
            "a deop under another capitalisation left the operator in place"
        );
    }

    #[test]
    fn a_command_that_changes_nothing_sends_nothing() {
        let env = env(&[("OPS", json!("Notch"))]);
        assert_eq!(sync_env(&command("op Notch"), &env, "s"), None);
        assert_eq!(sync_env(&command("deop Steve"), &env, "s"), None);
        assert_eq!(
            sync_env(&command("pardon Griefer"), &env, "s"),
            None,
            "nobody is banned, so there is nothing to pardon"
        );
    }

    /// The two lists are separate keys and must not cross: a ban written to
    /// `OPS` would op the griefer.
    #[test]
    fn bans_go_to_their_own_key() {
        let env = env(&[("OPS", json!("Notch")), ("BANNED", json!("Old"))]);
        let change = sync_env(&command("ban Griefer for griefing"), &env, "srv-1").expect("a ban");

        assert_eq!(change.list, List::Banned);
        assert_eq!(
            change.request.body,
            json!({ "environment_variables": { "BANNED": "Old,Griefer" } })
        );
        assert_eq!(
            change.line,
            "[Homerun] Ban change saved to server settings: banned=[Old, Griefer]"
        );
    }

    #[test]
    fn pardoning_removes_from_the_ban_list() {
        let env = env(&[("BANNED", json!("Griefer,Other"))]);
        let change = sync_env(&command("/pardon Griefer"), &env, "srv-1").expect("a pardon");
        assert_eq!(
            change.users,
            vec!["Other"],
            "the pardoned player is still banned"
        );
        assert_eq!(
            change.request.body,
            json!({ "environment_variables": { "BANNED": "Other" } })
        );
    }

    #[test]
    fn emptying_a_list_sends_an_empty_string_not_a_dropped_key() {
        let env = env(&[("OPS", json!("Notch"))]);
        let change = sync_env(&command("deop Notch"), &env, "srv-1").expect("a change");
        assert_eq!(
            change.request.body,
            json!({ "environment_variables": { "OPS": "" } }),
            "the key has to stay, or the next launch falls back to a stale list"
        );
    }

    // ─── reading the current list ───────────────────────────────────────────

    #[test]
    fn the_legacy_array_is_read_when_the_canonical_key_is_absent() {
        let env = env(&[
            ("op_users", json!(["Notch"])),
            ("banned_users", json!(["Griefer"])),
        ]);
        assert_eq!(
            sync_env(&command("op Notch"), &env, "s"),
            None,
            "op_users was not consulted, so an existing operator was re-added"
        );
        assert_eq!(
            sync_env(&command("pardon Griefer"), &env, "s")
                .expect("a pardon")
                .users,
            Vec::<String>::new()
        );
    }

    /// Key presence, not truthiness — `serverEnv.ts` records the bug. An
    /// emptied `OPS` means nobody, and falling through to the array would
    /// re-op whoever was just removed.
    #[test]
    fn an_empty_canonical_list_beats_a_stale_array() {
        let env = env(&[("OPS", json!("")), ("op_users", json!(["Removed"]))]);
        let change = sync_env(&command("op Notch"), &env, "srv-1").expect("a change");
        assert_eq!(
            change.users,
            vec!["Notch"],
            "the stale array resurrected a removed operator"
        );
    }

    #[test]
    fn a_server_with_no_environment_at_all_starts_the_list() {
        for server in [json!({}), json!({ "config": {} }), json!(null)] {
            let change = sync(&command("op Notch"), &server, "srv-1").expect("a change");
            assert_eq!(change.users, vec!["Notch"], "{server}");
        }
    }

    /// A host that guesses the wrong level reads an empty list and PATCHes away
    /// every existing operator.
    #[test]
    fn the_environment_is_read_from_the_config_the_api_returns() {
        let server = json!({
            "config": { "environment_variables": { "OPS": "Steve" } },
            "environment_variables": { "OPS": "WrongPlace" },
        });
        let change = sync(&command("op Notch"), &server, "srv-1").expect("a change");
        assert_eq!(
            change.users, vec!["Steve", "Notch"],
            "the environment was read from the wrong level, so an existing              operator would have been PATCHed away"
        );
    }

    #[test]
    fn the_capitalisation_the_operator_typed_is_what_is_stored() {
        let change = sync_env(&command("op NoTcH"), &json!({}), "srv-1").expect("a change");
        assert_eq!(change.users, vec!["NoTcH"]);
    }
}
