//! A minigame plugin's match report, lifted out of the console.
//!
//! Reference: `maybeReportMinigameStats` in `nativeServerManager.ts`.
//!
//! A server plugin prints one `[HOMERUN:STATS] {json}` line when a match ends.
//! The host is a courier: it recognises the line, namespaces the match id with
//! the server it came from — so the same plugin running on two servers, both
//! numbering matches from one, cannot have its reports collapse into each
//! other, and so a resend is idempotent — and posts it.
//!
//! Nothing here is required for a match to be played, which is why everything
//! that can go wrong ends as [`None`]. A plugin printing garbage, or a plugin
//! that was never a minigame plugin printing the marker by accident, must cost
//! the server nothing at all.

use crate::minecraft::console::strip_ansi;
use crate::reporting::Request;
use serde_json::Value;

/// What a plugin prints ahead of its JSON, trailing space included.
pub const MARKER: &str = "[HOMERUN:STATS] ";

const PATH: &str = "/api/minigame/stats/";

/// The report a console line carries, if it carries one.
///
/// `server_id` is this server's id in the API — both the namespace for the
/// match and the `server` field the ingest keys on.
pub fn from_console_line(server_id: &str, line: &str) -> Option<Request> {
    let (before, after) = line.split_once(MARKER)?;
    if !printed_by_the_server(before) {
        return None;
    }

    // One value, and whatever trails it is not our business: Paper ends a
    // coloured line with a reset sequence and a strict parse would throw the
    // whole match away over it. Stripping the tail instead would mean
    // rewriting the payload, and `[0m` is text a plugin may legitimately have
    // put inside a string.
    let value = serde_json::Deserializer::from_str(after)
        .into_iter::<Value>()
        .next()?
        .ok()?;
    let Value::Object(mut payload) = value else {
        return None;
    };

    // The desktop asks only that these three are truthy, which in JavaScript
    // also admits `0` and `[]`; these are the shapes that actually occur.
    let match_id = match payload.get("match") {
        Some(Value::String(id)) if !id.is_empty() => id.clone(),
        Some(Value::Number(id)) => id.to_string(),
        _ => return None,
    };
    if !matches!(payload.get("game"), Some(Value::String(game)) if !game.is_empty()) {
        return None;
    }
    if !matches!(payload.get("players"), Some(players) if !players.is_null()) {
        return None;
    }

    payload.insert(
        "match".into(),
        Value::String(format!("{server_id}:{match_id}")),
    );
    payload.insert("server".into(), Value::String(server_id.to_string()));
    Some(Request::post(PATH, Value::Object(payload)))
}

/// Whether the marker was printed by the server rather than quoted by someone
/// on it.
///
/// The desktop takes the marker from anywhere in the line, so anything the
/// server echoes — chat above all — can carry it. A player typing
/// `[HOMERUN:STATS] {"game":"x","match":"1","players":[…]}` in chat gets a
/// match of their own invention into the leaderboards, with no permission at
/// all. Same reasoning as `console::player_before`: the log prefix ends at the
/// first `]: ` and what follows on a real plugin line is the plugin's own
/// `[Tag] `, never an author.
///
/// It cannot be airtight. `/say` prints `[Name] text`, which is a logger tag
/// as far as the console is concerned — but `/say` is op-only, so what is left
/// is a griefer who was already trusted, rather than anyone who can type. A
/// log format with no `]: ` in it at all is treated as unrecognised and its
/// reports are dropped; that is the best-effort half of the bargain.
fn printed_by_the_server(before: &str) -> bool {
    let clean = strip_ansi(before);
    let mut rest = clean
        .split_once("]: ")
        .map_or(&*clean, |(_, message)| message);
    loop {
        rest = rest.trim_start();
        let Some(tag) = rest.strip_prefix('[') else {
            return rest.is_empty();
        };
        let Some((_, after)) = tag.split_once(']') else {
            return false;
        };
        rest = after;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporting::{Auth, Method};

    /// What a plugin actually prints through the Bukkit logger.
    const LINE: &str = concat!(
        "[20:41:07] [Server thread/INFO]: [SkyWars] [HOMERUN:STATS] ",
        r#"{"game":"skywars","match":"42","players":[{"name":"Notch","kills":3}],"duration":214}"#
    );

    #[test]
    fn a_finished_match_becomes_a_device_signed_post() {
        let request = from_console_line("srv-1", LINE).expect("a plugin's own stats line");
        assert_eq!(request.method, Method::Post);
        assert_eq!(request.path, "/api/minigame/stats/");
        // A user token here is a 403, or worse a report filed against whoever
        // happens to be signed in.
        assert_eq!(request.auth, Auth::Device);
        assert_eq!(request.body["game"], "skywars");
        assert_eq!(request.body["server"], "srv-1");
        assert_eq!(request.body["players"][0]["name"], "Notch");
        // Everything the plugin sent travels, not just the fields read here.
        assert_eq!(request.body["duration"], 214);
    }

    /// Two servers running the same plugin both call their first match "42".
    /// Reported as-is they are one match, and ingest keeps whichever landed
    /// last.
    #[test]
    fn the_match_id_carries_the_server_it_was_played_on() {
        let one = from_console_line("srv-1", LINE).expect("stats from the first server");
        let two = from_console_line("srv-2", LINE).expect("stats from the second server");
        assert_eq!(one.body["match"], "srv-1:42");
        assert_ne!(
            one.body["match"], two.body["match"],
            "two servers' match 42 arrived as one match, so one of them is gone"
        );
    }

    /// A line the plugin printed as `[Server thread/INFO]: <what follows>`.
    fn logged(rest: &str) -> Option<Request> {
        from_console_line("srv-1", &format!("[20:41:07] [Server thread/INFO]: {rest}"))
    }

    #[test]
    fn a_line_the_plugin_mangled_is_dropped_rather_than_raised() {
        // A truncated write, and what a truncated write leaves behind.
        assert!(logged(r#"[HOMERUN:STATS] {"game":"skywars","#).is_none());
        assert!(logged("[HOMERUN:STATS] ").is_none());
        // The marker with something that is not an object behind it.
        assert!(logged("[HOMERUN:STATS] ok").is_none());
        // And an ordinary line, which is every other line on the server.
        assert!(logged("Notch joined the game").is_none());
    }

    /// A report with no players in it is a row nobody can be credited for.
    #[test]
    fn a_report_missing_a_required_field_is_not_a_report() {
        let without = |json: &str| logged(&format!("[HOMERUN:STATS] {json}"));
        assert!(without(r#"{"game":"skywars","match":"42"}"#).is_none());
        assert!(without(r#"{"game":"skywars","players":[]}"#).is_none());
        assert!(without(r#"{"match":"42","players":[]}"#).is_none());
        assert!(without(r#"{"game":"skywars","match":null,"players":[]}"#).is_none());
        // Present but empty is the same as absent — it namespaces to "srv-1:".
        assert!(without(r#"{"game":"skywars","match":"","players":[]}"#).is_none());
    }

    /// Anyone on the server can type the marker into chat, and the server
    /// prints what they typed. Accepting that is a forged match, credited to
    /// whoever they name, from a player with no permissions at all.
    #[test]
    fn chat_cannot_forge_a_match() {
        let payload =
            r#"{"game":"skywars","match":"99","players":[{"name":"Griefer","kills":99}]}"#;
        for spoken in [
            format!("[20:41:07] [Server thread/INFO]: <Griefer> [HOMERUN:STATS] {payload}"),
            // With a log prefix of their own typed in front of the marker.
            format!("[20:41:07] [Server thread/INFO]: <Griefer> hey]: [HOMERUN:STATS] {payload}"),
            // /me, which needs no permission either.
            format!("[20:41:07] [Server thread/INFO]: * Griefer [HOMERUN:STATS] {payload}"),
        ] {
            assert!(
                from_console_line("srv-1", &spoken).is_none(),
                "a player typed a match into existence: {spoken}"
            );
        }
    }

    /// Paper colours plugin output and closes the line with a reset, so on a
    /// real server the JSON has a colour sequence stuck to the end of it.
    #[test]
    fn a_coloured_plugin_line_still_reports() {
        let line = concat!(
            "\u{1b}[m\u{1b}[36;22m[20:41:07 INFO]: \u{1b}[m\u{1b}[0;39;22m[SkyWars] [HOMERUN:STATS] ",
            r#"{"game":"skywars","match":"42","players":[]}"#,
            "\u{1b}[m"
        );
        let request = from_console_line("srv-1", line).expect("a coloured plugin line");
        assert_eq!(request.body["match"], "srv-1:42");
    }
}
