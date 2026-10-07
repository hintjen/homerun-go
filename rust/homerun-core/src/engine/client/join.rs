//! A server's join address, and the store link that carries it.
//!
//! Ported from the desktop's `gameRunner/joinUrl.ts`, which keeps working
//! until it calls this instead. The rules are its rules; the one place this
//! differs is said below.
//!
//! # Why the link's scheme is a constant
//!
//! A join link is handed to the OS, and the OS starts whatever program is
//! registered for its scheme: `ms-msdt:`, `search-ms:`, `file:`, every "open
//! in our app" handler on the machine. So the scheme is decided here, from
//! [`JOIN_URL_SCHEMES`], **before** anything is substituted, and a template
//! with any other scheme is never filled in.
//!
//! # Two inputs, two owners
//!
//! The template ships inside the signed build. The host and the ports come
//! from the API over the network, so they are validated as values, and a value
//! that would give the link a different meaning is refused, never escaped:
//! `evil.com/@good.example.com` and `user:pass@evil.com` are well-formed URL
//! text, which is exactly why parsing one is not a check.
//!
//! # The one difference from the TypeScript
//!
//! `joinUrl.ts` finishes by round-tripping the filled link through `new URL`
//! and refusing anything that comes back different. This crate has no URL
//! parser and is not taking one on for this. Instead the template's literal
//! text is held to [`literal_is_safe`]'s short character set, so a filled
//! link contains only characters no URL parser rewrites. That refuses a few
//! templates the round trip would have allowed and admits none it refused.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::engine::descriptor::Port;
use crate::tunnel::Protocol;

/// Schemes a join link may have.
///
/// Exactly one, deliberately. Adding one means deciding that every game's
/// descriptor may start that handler on a player's machine.
pub const JOIN_URL_SCHEMES: &[&str] = &["steam:"];

/// The longest template considered, matching `joinUrl.ts`.
const MAX_TEMPLATE: usize = 512;

/// Where players connect: a host with no port, and the public port per name.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct JoinAddress {
    pub host: String,
    /// Descriptor port name -> the gateway's public port for it.
    pub ports: BTreeMap<String, u16>,
}

/// Every refusal a player can see, with nothing from the template or the
/// API in it: a player cannot act on the difference between a bad scheme and
/// a stray placeholder, and a value somebody chose should not be echoed back.
pub const CANNOT_OPEN: &str =
    "Homerun could not open this game's join link. Copy the address and join from the game instead.";
pub const NO_LINK: &str = "This game does not provide a way to join from Homerun.";
pub const NO_PUBLIC_PORT: &str =
    "Homerun does not have a public port for this server yet. Wait for it to finish starting, then try again.";

/// A host safe to substitute: no separator, no credential, no whitespace.
///
/// Narrower than DNS allows on purpose. Everything that gives a URL a
/// different meaning (`/`, `?`, `#`, `@`, `:`, `%`, `\`, a space, a control
/// character, anything non-ASCII) is absent from the set, not removed from
/// the value.
pub fn is_join_host(value: &str) -> bool {
    (1..=253).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
}

/// The host and public ports for a server, from the link the API returned.
///
/// `domain.uri` before `fqdn`: when a game sets `client.srv`, `fqdn` is the
/// bare SRV name, which routes only for a client that follows the SRV record,
/// and a store link does a plain lookup.
///
/// `forward_ports` is keyed by gateway service, with entries
/// `"<public>:<dest>/<proto>"`. An entry with no `public:` half is a port the
/// gateway has not assigned yet, and is left out: its number is the port the
/// server binds, and a player sent there arrives nowhere.
pub fn address_from_link(ports: &[Port], link: &Value) -> Option<JoinAddress> {
    let link = link.as_object()?;
    let host = match link
        .get("domain")
        .and_then(|d| d.get("uri"))
        .and_then(Value::as_str)
    {
        Some(uri) if !uri.is_empty() => uri.to_string(),
        _ => host_of(link.get("fqdn")?.as_str()?).to_string(),
    };
    if !is_join_host(&host) {
        return None;
    }

    let forwards = link.get("forward_ports").and_then(Value::as_object);
    let mut resolved = BTreeMap::new();
    for port in ports {
        if !port.expose {
            continue;
        }
        let Some(service) = &port.service else {
            continue;
        };
        let Some(entries) = forwards
            .and_then(|f| f.get(service))
            .and_then(Value::as_array)
        else {
            continue;
        };
        if let Some(public) = public_port_for(entries, port) {
            resolved.insert(port.name.clone(), public);
        }
    }
    Some(JoinAddress {
        host,
        ports: resolved,
    })
}

/// `gw.example.com:20011` -> `gw.example.com`; a bare host is unchanged.
fn host_of(fqdn: &str) -> &str {
    match fqdn.rsplit_once(':') {
        Some((host, port))
            if !host.is_empty() && !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) =>
        {
            host
        }
        _ => fqdn,
    }
}

/// `["20011:28015/udp"]` and the `28015/udp` port -> `20011`.
fn public_port_for(entries: &[Value], port: &Port) -> Option<u16> {
    let proto = match port.proto {
        Protocol::Tcp => "tcp",
        Protocol::Udp => "udp",
    };
    let wanted = format!("{}/{proto}", port.port);
    entries.iter().filter_map(Value::as_str).find_map(|entry| {
        // No `public:` half means the gateway has not assigned one yet.
        let (public, dest) = entry.split_once(':')?;
        if dest != wanted || !public.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        public.parse::<u16>().ok().filter(|p| *p != 0)
    })
}

/// Fill a join template, or say why not, in a sentence for a player.
///
/// Never returns a link whose scheme is outside [`JOIN_URL_SCHEMES`].
pub fn build_join_url(template: &str, address: &JoinAddress) -> Result<String, &'static str> {
    if template.is_empty() || template.len() > MAX_TEMPLATE {
        return Err(NO_LINK);
    }
    // The scheme is decided before anything is filled in. Exact and
    // lower-case: `STEAM://` is a link this build was not asked to open.
    let scheme = JOIN_URL_SCHEMES
        .iter()
        .find(|s| template.starts_with(**s))
        .ok_or(CANNOT_OPEN)?;

    let mut out = String::with_capacity(template.len() + 32);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let literal = &rest[..open];
        if !literal_is_safe(literal) {
            return Err(CANNOT_OPEN);
        }
        out.push_str(literal);
        let after = &rest[open + 1..];
        let close = after.find('}').ok_or(CANNOT_OPEN)?;
        let name = &after[..close];
        if name == "host" {
            // Checked here as well as when the address was read, because an
            // address can be built by hand.
            if !is_join_host(&address.host) {
                return Err(CANNOT_OPEN);
            }
            out.push_str(&address.host);
        } else if let Some(port_name) = name.strip_prefix("port:") {
            if !port_name_is_plain(port_name) {
                return Err(CANNOT_OPEN);
            }
            match address.ports.get(port_name) {
                Some(port) if *port != 0 => out.push_str(&port.to_string()),
                _ => return Err(NO_PUBLIC_PORT),
            }
        } else {
            // `{secret:...}`, `{setting:...}`, `{serverDir}`, `{HOST}`, `{}`:
            // none of them belongs in something shown to players.
            return Err(CANNOT_OPEN);
        }
        rest = &after[close + 1..];
    }
    if !literal_is_safe(rest) {
        return Err(CANNOT_OPEN);
    }
    out.push_str(rest);
    debug_assert!(out.starts_with(scheme));
    Ok(out)
}

/// The most elements a `joinArgs` template may have. Terraria's has four.
const MAX_ARGS: usize = 16;

/// Fill a `client.joinArgs` template: the launch arguments that make a game
/// join a server, which Steam passes to it with `-applaunch`.
///
/// # Every element is one of three shapes, or it is refused
///
/// - **A flag:** `-` or `+`, a letter, then letters, digits, `_`, `.`, `-`.
///   `-join`, `-port`, `+connect`. Not a value, ever: values come from the
///   address, so a descriptor cannot smuggle a path or a second command in.
/// - **`{host}` or `{port:<name>}`**, the whole element.
/// - **Both, joined by `:`** (`{host}:{port:game}`), for a game that takes one
///   `host:port` argument.
///
/// A filled value never begins with `-` or `+`, so the game cannot read the
/// server's address as a flag: a host is held to [`is_join_host`] and must
/// not start with either. The program and the arguments before these are the
/// core's own (`steam.exe -silent -applaunch <appId>`), never the
/// descriptor's.
pub fn build_join_args(
    template: &[String],
    address: &JoinAddress,
) -> Result<Vec<String>, &'static str> {
    check_join_args(template).map_err(|_| CANNOT_OPEN)?;
    template
        .iter()
        .map(|element| {
            if is_flag(element) {
                Ok(element.clone())
            } else {
                fill_value(element, address)
            }
        })
        .collect()
}

/// The shape check alone, for validation, which has no address to fill in.
/// `Err` names the first element that is not one of the three shapes.
pub fn check_join_args(template: &[String]) -> Result<(), String> {
    if template.is_empty() || template.len() > MAX_ARGS {
        return Err(format!("client.joinArgs has 1 to {MAX_ARGS} elements"));
    }
    for element in template {
        if !is_flag(element) && value_placeholders(element).is_none() {
            return Err(format!(
                "client.joinArgs element \"{element}\" is not a flag, {{host}}, {{port:<name>}} or the two joined by ':'"
            ));
        }
    }
    Ok(())
}

/// The port names a template uses, for validation to check they are declared.
pub fn join_args_ports(template: &[String]) -> Vec<String> {
    template
        .iter()
        .filter_map(|e| value_placeholders(e))
        .flatten()
        .filter_map(|p| p.strip_prefix("port:").map(str::to_string))
        .collect()
}

fn is_flag(element: &str) -> bool {
    let mut bytes = element.bytes();
    matches!(bytes.next(), Some(b'-' | b'+'))
        && matches!(bytes.next(), Some(b) if b.is_ascii_alphabetic())
        && element.len() <= 32
        && bytes.all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

/// The placeholder names of a value element (`host`, `port:<name>`): one
/// whole-element placeholder, or two joined by `:`. `None` for anything else.
fn value_placeholders(element: &str) -> Option<Vec<String>> {
    fn single(part: &str) -> Option<String> {
        let name = part.strip_prefix('{')?.strip_suffix('}')?;
        let ok = name == "host" || name.strip_prefix("port:").is_some_and(port_name_is_plain);
        ok.then(|| name.to_string())
    }
    if let Some(one) = single(element) {
        return Some(vec![one]);
    }
    // `{host}:{port:game}`: split at the `}:{` between the two halves.
    let (left, right) = element.split_once("}:{")?;
    Some(vec![
        single(&format!("{left}}}"))?,
        single(&format!("{{{right}"))?,
    ])
}

fn fill_value(element: &str, address: &JoinAddress) -> Result<String, &'static str> {
    let names = value_placeholders(element).ok_or(CANNOT_OPEN)?;
    let mut parts = Vec::with_capacity(names.len());
    for name in names {
        if name == "host" {
            // Never a flag: a host the game read as `-something` would be one.
            if !is_join_host(&address.host) || address.host.starts_with('-') {
                return Err(CANNOT_OPEN);
            }
            parts.push(address.host.clone());
        } else {
            let port_name = name.strip_prefix("port:").ok_or(CANNOT_OPEN)?;
            match address.ports.get(port_name) {
                Some(port) if *port != 0 => parts.push(port.to_string()),
                _ => return Err(NO_PUBLIC_PORT),
            }
        }
    }
    Ok(parts.join(":"))
}

/// Template text outside placeholders: letters, digits and the URL
/// punctuation a connect link uses. No `@`, `#`, `\`, `}`, whitespace or
/// control character, and nothing non-ASCII.
fn literal_is_safe(text: &str) -> bool {
    text.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b":/._~-+%=&?".contains(&b))
}

fn port_name_is_plain(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    //! The cases are `gameRunner.joinUrl.test.ts`'s, so the two copies are
    //! held to the same rules until the TypeScript one goes.

    use super::*;
    use serde_json::json;

    const TEMPLATE: &str = "steam://connect/{host}:{port:game}";

    fn address() -> JoinAddress {
        JoinAddress {
            host: "us-east.gethomerun.app".into(),
            ports: BTreeMap::from([("game".into(), 20011), ("query".into(), 20012)]),
        }
    }

    fn ports() -> Vec<Port> {
        vec![
            Port {
                name: "game".into(),
                proto: Protocol::Udp,
                port: 28015,
                expose: true,
                service: Some("game".into()),
            },
            Port {
                name: "query".into(),
                proto: Protocol::Udp,
                port: 28017,
                expose: true,
                service: Some("game".into()),
            },
            Port {
                name: "rcon".into(),
                proto: Protocol::Tcp,
                port: 28016,
                expose: false,
                service: None,
            },
        ]
    }

    #[test]
    fn the_allowlist_is_exactly_steam() {
        assert_eq!(JOIN_URL_SCHEMES, &["steam:"]);
    }

    #[test]
    fn a_scheme_off_the_allowlist_is_refused_before_anything_is_filled_in() {
        for template in [
            "https://example.com/join?host={host}",
            "http://example.com/{host}",
            "file:///C:/Windows/System32/cmd.exe",
            "javascript:alert({port:game})",
            "data:text/html,<script>1</script>",
            "vbscript:msgbox",
            "ms-msdt:/id%20PCWDiagnostic",
            "search-ms:query={host}",
            "ms-officecmd:{host}",
            "shell:startup",
            "STEAM://connect/{host}:{port:game}",
            "steamx://connect/{host}",
            "//connect/{host}",
            "connect/{host}:{port:game}",
        ] {
            assert_eq!(
                build_join_url(template, &address()),
                Err(CANNOT_OPEN),
                "{template}"
            );
        }
    }

    #[test]
    fn a_placeholder_it_may_not_have_is_refused() {
        for template in [
            "steam://connect/{host}:{port:game}?password={secret:rcon}",
            "steam://connect/{secret:admin}",
            "steam://connect/{host}:{port:game}?name={setting:hostname}",
            "steam://connect/{host}:{port:game}/{serverDir}",
            "steam://connect/{host}?n={serverName}",
            "steam://connect/{}",
            "steam://connect/{host}:{port:}",
            "steam://connect/{host}:{port:game/../rcon}",
            "steam://connect/{HOST}:{port:game}",
            "steam://connect/{host",
        ] {
            assert!(build_join_url(template, &address()).is_err(), "{template}");
        }
    }

    #[test]
    fn a_port_the_gateway_has_not_assigned_is_named_not_guessed() {
        let only_game = JoinAddress {
            host: "us-east.gethomerun.app".into(),
            ports: BTreeMap::from([("game".into(), 20011)]),
        };
        assert_eq!(
            build_join_url("steam://connect/{host}:{port:query}", &only_game),
            Err(NO_PUBLIC_PORT)
        );
        let zero = JoinAddress {
            ports: BTreeMap::from([("game".into(), 0)]),
            ..address()
        };
        assert_eq!(build_join_url(TEMPLATE, &zero), Err(NO_PUBLIC_PORT));
    }

    #[test]
    fn a_hostile_host_never_reaches_the_link() {
        for host in [
            "evil.com/@good.example.com",
            "evil.com/",
            "a b",
            "x?y",
            "x#y",
            "%0a",
            "host%00",
            "host\nHeader: 1",
            "host\\..\\..",
            "user:pass@evil.com",
            "evil.com:1234",
            "[::1]",
            "xn--evil\u{3002}com",
            "evíl.com",
            "",
            " ",
        ] {
            assert!(!is_join_host(host), "{host:?}");
            let hostile = JoinAddress {
                host: host.into(),
                ..address()
            };
            assert_eq!(
                build_join_url(TEMPLATE, &hostile),
                Err(CANNOT_OPEN),
                "{host:?}"
            );
        }
        for host in [
            "us-east.gethomerun.app",
            "1.2.3.4",
            "a",
            "HOST.Example.COM",
            "a-b.c-d.example",
        ] {
            assert!(is_join_host(host), "{host}");
        }
    }

    #[test]
    fn the_happy_path_is_exactly_the_address() {
        assert_eq!(
            build_join_url(TEMPLATE, &address()).unwrap(),
            "steam://connect/us-east.gethomerun.app:20011"
        );
        assert_eq!(
            build_join_url(
                "steam://connect/{host}:{port:game}?q={port:query}&h={host}",
                &address()
            )
            .unwrap(),
            "steam://connect/us-east.gethomerun.app:20011?q=20012&h=us-east.gethomerun.app"
        );
    }

    #[test]
    fn a_template_that_is_not_one_is_refused() {
        for template in [
            "",
            "   ",
            &format!("steam://connect/{}", "x".repeat(600)),
            "steam://connect/{host}\t:{port:game}",
            "steam://connect/{host}\n",
            "steam://connect/{host} {port:game}",
            "steam://connect/{host}\u{0}",
            "steam://connect/{host}@evil",
            "steam://connect/{host}#frag",
            "steam://connect/{host}}",
        ] {
            assert!(
                build_join_url(template, &address()).is_err(),
                "{template:?}"
            );
        }
    }

    fn link(overrides: Value) -> Value {
        let mut base = json!({
            "fqdn": "us-east.gethomerun.app:20011",
            "forward_ports": { "game": ["20011:28015/udp", "20012:28017/udp"] }
        });
        for (k, v) in overrides.as_object().unwrap() {
            base[k] = v.clone();
        }
        base
    }

    #[test]
    fn each_exposed_port_maps_to_its_public_port() {
        assert_eq!(
            address_from_link(&ports(), &link(json!({}))),
            Some(address())
        );
    }

    #[test]
    fn domain_uri_wins_because_an_srv_name_answers_no_plain_lookup() {
        let a = address_from_link(
            &ports(),
            &link(json!({ "fqdn": "flat-srv-name", "domain": { "uri": "gw.example.com" } })),
        );
        assert_eq!(a.unwrap().host, "gw.example.com");
    }

    #[test]
    fn a_bare_entry_is_not_provisioned_and_its_number_is_never_used() {
        let a = address_from_link(
            &ports(),
            &link(json!({ "forward_ports": { "game": ["28015/udp", "20012:28017/udp"] } })),
        )
        .unwrap();
        assert_eq!(a.ports, BTreeMap::from([("query".into(), 20012)]));

        let none = address_from_link(
            &ports(),
            &link(json!({ "forward_ports": { "game": ["28015/udp", "28017/udp"] } })),
        )
        .unwrap();
        assert!(none.ports.is_empty());
        assert_eq!(build_join_url(TEMPLATE, &none), Err(NO_PUBLIC_PORT));
    }

    #[test]
    fn an_unexposed_port_or_the_wrong_protocol_is_never_mapped() {
        let a = address_from_link(
            &ports(),
            &link(json!({ "forward_ports": { "game": ["20011:28015/udp", "20016:28016/tcp"] } })),
        )
        .unwrap();
        assert_eq!(a.ports, BTreeMap::from([("game".into(), 20011)]));

        let tcp = address_from_link(
            &ports(),
            &link(json!({ "forward_ports": { "game": ["20011:28015/tcp"] } })),
        )
        .unwrap();
        assert!(tcp.ports.is_empty());
    }

    #[test]
    fn a_link_with_no_usable_host_is_none() {
        for raw in [
            Value::Null,
            json!("us-east.gethomerun.app"),
            json!({ "forward_ports": { "game": ["20011:28015/udp"] } }),
            json!({ "fqdn": "evil.com/@x", "forward_ports": {} }),
            json!({ "domain": { "uri": "a b" }, "fqdn": "good.example.com" }),
        ] {
            assert_eq!(address_from_link(&ports(), &raw), None, "{raw}");
        }
    }

    #[test]
    fn a_bare_fqdn_is_kept() {
        let a = address_from_link(&ports(), &link(json!({ "fqdn": "us-east.gethomerun.app" })))
            .unwrap();
        assert_eq!(a.host, "us-east.gethomerun.app");
    }

    fn args(elements: &[&str]) -> Vec<String> {
        elements.iter().map(|e| e.to_string()).collect()
    }

    /// Terraria's, which a person saw join on 2026-10-07.
    #[test]
    fn terrarias_join_arguments_are_filled_in() {
        assert_eq!(
            build_join_args(
                &args(&["-join", "{host}", "-port", "{port:game}"]),
                &address()
            )
            .unwrap(),
            ["-join", "us-east.gethomerun.app", "-port", "20011"]
        );
        assert_eq!(
            build_join_args(&args(&["+connect", "{host}:{port:game}"]), &address()).unwrap(),
            ["+connect", "us-east.gethomerun.app:20011"]
        );
    }

    #[test]
    fn an_element_that_is_not_a_flag_or_a_placeholder_is_refused() {
        for bad in [
            &["-join", "C:\\Windows\\System32\\cmd.exe"][..],
            &["-join", "{host}", "-exec", "calc"],
            &["-join", "{secret:rcon}"],
            &["-join", "{setting:hostname}"],
            &["-join", "{serverDir}"],
            &["-join", "x{host}"],
            &["-join", "{host}:{port:game}:{port:query}"],
            &["join"],
            &["-"],
            &["--"],
            &["-1"],
            &["-a b"],
            &["-a\"b"],
            &[],
        ] {
            assert!(check_join_args(&args(bad)).is_err(), "{bad:?}");
            assert_eq!(
                build_join_args(&args(bad), &address()),
                Err(CANNOT_OPEN),
                "{bad:?}"
            );
        }
        assert!(
            check_join_args(&args(&["-a"; 17])).is_err(),
            "at most sixteen"
        );
    }

    #[test]
    fn a_host_that_reads_as_a_flag_or_a_missing_port_is_refused() {
        let flagged = JoinAddress {
            host: "-password".into(),
            ..address()
        };
        assert_eq!(
            build_join_args(&args(&["-join", "{host}"]), &flagged),
            Err(CANNOT_OPEN)
        );
        assert_eq!(
            build_join_args(&args(&["-port", "{port:nope}"]), &address()),
            Err(NO_PUBLIC_PORT)
        );
    }

    #[test]
    fn the_ports_a_template_uses_are_listed_for_validation() {
        assert_eq!(
            join_args_ports(&args(&[
                "-join",
                "{host}",
                "-port",
                "{port:game}",
                "{host}:{port:query}"
            ])),
            ["game", "query"]
        );
    }
}
