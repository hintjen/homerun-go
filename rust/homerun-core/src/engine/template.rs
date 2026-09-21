//! `{setting:…}`, `{port:…}`, `{secret:…}`, `{bindAddress}`, `{runtimeDir}` —
//! and the one rule that makes them safe.
//!
//! # Substitution happens once, and never looks at what it produced
//!
//! This is the whole security property of the module, so it is stated before
//! anything else.
//!
//! A placeholder is replaced by its value, and that value is **never scanned
//! again**. It is opaque text from the moment it lands. The reason is
//! immediate: a player names their server
//!
//! ```text
//! {secret:rcon}
//! ```
//!
//! and if substitution re-scanned substituted text, the next pass would
//! resolve that placeholder and the server's RCON password would become its
//! public hostname — advertised in a server browser, printed in a log, and
//! visible to everyone who can see the server at all. There is a test named
//! for exactly that value.
//!
//! The same reasoning is why a *setting's* value is never templated. Only the
//! descriptor's own strings — argv, env, config values, the join URL — are,
//! and those are authored by us and shipped inside a signed host build. The
//! one exception is a setting's `default`, which is also descriptor-authored,
//! and which may contain `{serverName}` and nothing else; that is resolved in
//! [`super::settings`], before the value becomes player data.
//!
//! # An unset setting removes its flag
//!
//! `{setting:seed}` with no seed does not become an empty argument — an empty
//! argument is a value, and `+server.seed ""` is not the same launch line as
//! one with no seed in it. The token is dropped, and so is the `+flag` in
//! front of it. [`super::invocation`] does the dropping; this module reports
//! that a string resolved to nothing.
//!
//! # `{bindAddress}` is how a promise becomes an argument
//!
//! The descriptor says which ports are `expose: false`, and the host binds
//! them on loopback only. That was a promise nothing kept: the address was
//! validated, then dropped, and the game bound wherever it pleased. The
//! placeholder is what hands the address to the server, and
//! [`super::validate`] warns about a descriptor with an administrative
//! console that never uses it.
//!
//! # Braces that are not placeholders
//!
//! `{{` is a literal `{`. Anything else between braces must be a placeholder
//! this module knows, or it is a validation error — never an empty string.
//! Silently dropping an unknown placeholder is how a server starts with
//! `+server.hostname` and no name after it.

use std::collections::BTreeMap;

use super::settings::{render, Resolved};
use crate::{Error, Result};

/// Everything a placeholder can refer to.
///
/// Ports are the *bound* ports, not the descriptor's preferred ones: the
/// runner may have had to move off a taken port, and an argument naming the
/// port the server did not bind is a server nobody can reach.
#[derive(Debug, Clone, Copy)]
pub struct Bindings<'a> {
    pub settings: &'a Resolved,
    pub ports: &'a BTreeMap<String, u16>,
    pub secrets: &'a BTreeMap<String, String>,
    pub server_name: &'a str,
    pub server_dir: &'a str,
    /// The address the server is being told to bind, for `{bindAddress}`.
    ///
    /// The host decides it; today every descriptor-driven launch is
    /// `127.0.0.1`, because the tunnel connects to loopback and a port the
    /// descriptor marks `expose: false` has no business anywhere else. It is
    /// a binding rather than a constant so that the promise is *passed to the
    /// game* instead of merely being believed about it — a descriptor with no
    /// `{bindAddress}` in its launch line is a descriptor whose server binds
    /// wherever it likes.
    pub bind_address: &'a str,
    /// This game's installed files, for `{runtimeDir}`.
    ///
    /// Shared by every server of the game on the machine, which is exactly
    /// why it is a different binding from `{serverDir}` rather than a path
    /// built out of it.
    pub runtime_dir: &'a str,
}

/// What a string resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filled {
    /// Every placeholder replaced, exactly once each.
    Text(String),
    /// A `{setting:…}` in this string was unset, so the string is not an
    /// argument at all. See the module header.
    Dropped,
}

impl Filled {
    /// The text, or `None` if the string dropped out.
    pub fn text(&self) -> Option<&str> {
        match self {
            Filled::Text(t) => Some(t),
            Filled::Dropped => None,
        }
    }
}

/// One placeholder, parsed but not resolved.
///
/// [`super::validate`] walks these to check a descriptor without needing a
/// running server to resolve them against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placeholder {
    Setting(String),
    Port(String),
    Secret(String),
    ServerName,
    ServerDir,
    /// This game's installed files, shared by every server of it.
    RuntimeDir,
    /// The address the server is told to bind. Host-side only: it is a fact
    /// about this machine, not about how a player reaches the server.
    BindAddress,
    /// The gateway address a player connects to. Only meaningful in
    /// `client.joinUrl`, which the API fills in — the engine never resolves
    /// it, and [`fill`] refuses it so that a launch line cannot quietly
    /// depend on something this side does not know.
    Host,
}

impl Placeholder {
    /// How it appears in a descriptor, for error messages.
    pub fn spelling(&self) -> String {
        match self {
            Placeholder::Setting(k) => format!("{{setting:{k}}}"),
            Placeholder::Port(n) => format!("{{port:{n}}}"),
            Placeholder::Secret(n) => format!("{{secret:{n}}}"),
            Placeholder::ServerName => "{serverName}".into(),
            Placeholder::ServerDir => "{serverDir}".into(),
            Placeholder::RuntimeDir => "{runtimeDir}".into(),
            Placeholder::BindAddress => "{bindAddress}".into(),
            Placeholder::Host => "{host}".into(),
        }
    }
}

/// Split a string into its literal parts and its placeholders, once.
///
/// The single pass lives here: every caller consumes this and none of them
/// feeds the result back in.
fn scan(input: &str) -> Result<Vec<Piece>> {
    let mut pieces = Vec::new();
    let mut literal = String::new();
    let mut rest = input;

    while let Some(open) = rest.find('{') {
        literal.push_str(&rest[..open]);
        let after = &rest[open + 1..];

        // `{{` is a literal brace. Nothing else escapes, because nothing else
        // has needed to.
        if let Some(stripped) = after.strip_prefix('{') {
            literal.push('{');
            rest = stripped;
            continue;
        }

        let Some(close) = after.find('}') else {
            // An unclosed brace cannot be a placeholder, so it is text.
            literal.push('{');
            rest = after;
            continue;
        };

        let body = &after[..close];
        let placeholder = parse(body, input)?;
        if !literal.is_empty() {
            pieces.push(Piece::Literal(std::mem::take(&mut literal)));
        }
        pieces.push(Piece::Placeholder(placeholder));
        rest = &after[close + 1..];
    }

    literal.push_str(rest);
    if !literal.is_empty() {
        pieces.push(Piece::Literal(literal));
    }
    Ok(pieces)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Literal(String),
    Placeholder(Placeholder),
}

fn parse(body: &str, whole: &str) -> Result<Placeholder> {
    let unknown = || {
        Error::Malformed(format!(
            "this game's descriptor uses \"{{{body}}}\" in \"{whole}\", which is not \
             a placeholder Homerun knows. The file is part of the app, so this is a \
             bug in Homerun rather than something you can fix."
        ))
    };

    match body.split_once(':') {
        Some(("setting", key)) if !key.is_empty() => Ok(Placeholder::Setting(key.to_string())),
        Some(("port", name)) if !name.is_empty() => Ok(Placeholder::Port(name.to_string())),
        Some(("secret", name)) if !name.is_empty() => Ok(Placeholder::Secret(name.to_string())),
        Some(_) => Err(unknown()),
        None => match body {
            "serverName" => Ok(Placeholder::ServerName),
            "serverDir" => Ok(Placeholder::ServerDir),
            "runtimeDir" => Ok(Placeholder::RuntimeDir),
            "bindAddress" => Ok(Placeholder::BindAddress),
            "host" => Ok(Placeholder::Host),
            _ => Err(unknown()),
        },
    }
}

/// Every placeholder in a string, in order. Literal text is discarded.
pub fn placeholders(input: &str) -> Result<Vec<Placeholder>> {
    Ok(scan(input)?
        .into_iter()
        .filter_map(|p| match p {
            Piece::Placeholder(p) => Some(p),
            Piece::Literal(_) => None,
        })
        .collect())
}

/// Whether a string is one placeholder and nothing else.
///
/// The distinction [`super::invocation`] needs: `"{setting:seed}"` dropping
/// out takes its `+server.seed` with it, while `"seed={setting:seed}"`
/// dropping out takes only itself — there is no flag in front of it to drop,
/// and removing the previous argument would be removing something unrelated.
pub fn is_sole_placeholder(input: &str) -> bool {
    matches!(scan(input).as_deref(), Ok([Piece::Placeholder(_)]))
}

/// Whether a token is a bare flag — something an unset value's argument would
/// have been the value *for*.
///
/// A token carrying a placeholder is never a flag, however it starts: it is a
/// value that happens to resolve to something beginning with a dash.
pub fn looks_like_flag(token: &str) -> bool {
    (token.starts_with('+') || token.starts_with('-')) && !token.contains('{')
}

/// Resolve every placeholder in a descriptor-authored string.
///
/// Substitution is single-pass by construction: [`scan`] has already decided
/// what is a placeholder and what is literal, and the values written in below
/// are never handed back to it.
pub fn fill(input: &str, bindings: &Bindings) -> Result<Filled> {
    let mut out = String::new();
    let mut dropped = false;

    for piece in scan(input)? {
        match piece {
            Piece::Literal(text) => out.push_str(&text),
            Piece::Placeholder(p) => match &p {
                Placeholder::Setting(key) => {
                    let value = bindings.settings.get(key).ok_or_else(|| {
                        Error::Malformed(format!(
                            "this game's descriptor refers to a setting called \
                             \"{key}\" that it does not declare."
                        ))
                    })?;
                    if value.is_null() {
                        dropped = true;
                    } else {
                        // Opaque from here. Whatever this is, it is not
                        // rescanned -- see the module header.
                        out.push_str(&render(value));
                    }
                }
                Placeholder::Port(name) => {
                    let port = bindings.ports.get(name).ok_or_else(|| {
                        Error::Malformed(format!(
                            "this server has no port called \"{name}\" to start with."
                        ))
                    })?;
                    out.push_str(&port.to_string());
                }
                Placeholder::Secret(name) => {
                    let secret = bindings.secrets.get(name).ok_or_else(|| {
                        Error::Malformed(format!(
                            "this server has no \"{name}\" password to start with."
                        ))
                    })?;
                    out.push_str(secret);
                }
                Placeholder::ServerName => out.push_str(bindings.server_name),
                Placeholder::ServerDir => out.push_str(bindings.server_dir),
                Placeholder::RuntimeDir => {
                    if bindings.runtime_dir.is_empty() {
                        return Err(Error::Malformed(
                            "The host has not supplied this game's runtimeDir.".into(),
                        ));
                    }
                    out.push_str(bindings.runtime_dir);
                }
                Placeholder::BindAddress => out.push_str(bindings.bind_address),
                Placeholder::Host => {
                    return Err(Error::Malformed(format!(
                        "this game's descriptor uses {} where the server runs, but the \
                         address a player connects to is only known to Homerun's \
                         servers. The file is part of the app, so this is a bug in \
                         Homerun rather than something you can fix.",
                        p.spelling()
                    )))
                }
            },
        }
    }

    Ok(if dropped {
        Filled::Dropped
    } else {
        Filled::Text(out)
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn runtime_dir_is_a_literal_host_binding() {
        let f = Fixture::new();
        assert_eq!(
            fill("{runtimeDir}/Bundles", &f.bindings()).unwrap(),
            Filled::Text(format!("{}/Bundles", f.runtime_dir))
        );
        let mut missing = f.bindings();
        missing.runtime_dir = "";
        assert!(fill("{runtimeDir}", &missing).is_err());
    }
    use super::*;
    use serde_json::{json, Value};

    struct Fixture {
        settings: Resolved,
        ports: BTreeMap<String, u16>,
        secrets: BTreeMap<String, String>,
        server_name: String,
        server_dir: String,
        bind_address: String,
        runtime_dir: String,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                settings: [
                    ("hostname".to_string(), json!("Ruined Keep")),
                    ("maxPlayers".to_string(), json!(25)),
                    ("pve".to_string(), json!(true)),
                    ("seed".to_string(), Value::Null),
                ]
                .into_iter()
                .collect(),
                ports: [("game".to_string(), 28015u16), ("rcon".to_string(), 28016)]
                    .into_iter()
                    .collect(),
                secrets: [("rcon".to_string(), "hunter2".to_string())]
                    .into_iter()
                    .collect(),
                server_name: "Justin's server".into(),
                server_dir: "C:\\servers\\abc".into(),
                bind_address: "127.0.0.1".into(),
                runtime_dir: r"C:\runtime\rust".into(),
            }
        }

        fn bindings(&self) -> Bindings<'_> {
            Bindings {
                settings: &self.settings,
                ports: &self.ports,
                secrets: &self.secrets,
                server_name: &self.server_name,
                server_dir: &self.server_dir,
                bind_address: &self.bind_address,
                runtime_dir: &self.runtime_dir,
            }
        }
    }

    fn filled(input: &str) -> Filled {
        let f = Fixture::new();
        fill(input, &f.bindings()).unwrap()
    }

    fn text(input: &str) -> String {
        match filled(input) {
            Filled::Text(t) => t,
            Filled::Dropped => panic!("{input} unexpectedly dropped"),
        }
    }

    // ─── the property the module exists for ────────────────────────────────

    /// A player names their server `{secret:rcon}`.
    ///
    /// If substitution ever re-scanned what it substituted, this would put
    /// the server's RCON password into its public hostname. The assertion is
    /// that the password does not appear in the output *anywhere*, not merely
    /// that the output is some expected string.
    #[test]
    fn a_server_named_after_a_placeholder_does_not_leak_the_password() {
        let mut f = Fixture::new();
        f.settings.insert("hostname".into(), json!("{secret:rcon}"));

        let got = fill("+server.hostname={setting:hostname}", &f.bindings()).unwrap();
        let out = got.text().unwrap();

        assert!(
            !out.contains("hunter2"),
            "the RCON password reached the hostname: {out}"
        );
        assert_eq!(out, "+server.hostname={secret:rcon}");
    }

    /// The same, one level further out: the *server name* itself is player
    /// data and is equally not rescanned.
    #[test]
    fn a_hostile_server_name_is_literal_text() {
        let mut f = Fixture::new();
        f.server_name = "{secret:rcon}".into();
        let got = fill("{serverName}", &f.bindings()).unwrap();
        assert_eq!(got.text().unwrap(), "{secret:rcon}");
    }

    /// A value that looks like a *different* placeholder is equally inert,
    /// including one that would otherwise have expanded to a path.
    #[test]
    fn a_setting_that_looks_like_any_placeholder_stays_inert() {
        let mut f = Fixture::new();
        f.settings.insert(
            "hostname".into(),
            json!("{serverDir} {port:rcon} {setting:pve}"),
        );
        let got = fill("{setting:hostname}", &f.bindings()).unwrap();
        assert_eq!(got.text().unwrap(), "{serverDir} {port:rcon} {setting:pve}");
    }

    // ─── ordinary substitution ─────────────────────────────────────────────

    #[test]
    fn each_kind_of_placeholder_resolves() {
        assert_eq!(text("{setting:hostname}"), "Ruined Keep");
        assert_eq!(text("{setting:maxPlayers}"), "25");
        assert_eq!(text("{setting:pve}"), "true");
        assert_eq!(text("{port:game}"), "28015");
        assert_eq!(text("{secret:rcon}"), "hunter2");
        assert_eq!(text("{serverName}"), "Justin's server");
        assert_eq!(text("{serverDir}"), "C:\\servers\\abc");
    }

    #[test]
    fn placeholders_can_be_mixed_with_literal_text() {
        assert_eq!(
            text("--motd=Welcome to {setting:hostname} ({port:game})"),
            "--motd=Welcome to Ruined Keep (28015)"
        );
    }

    #[test]
    fn a_string_with_nothing_in_it_comes_back_unchanged() {
        assert_eq!(text("-batchmode"), "-batchmode");
        assert_eq!(text(""), "");
    }

    #[test]
    fn a_doubled_brace_is_a_literal_brace() {
        assert_eq!(text("{{setting:hostname}"), "{setting:hostname}");
        assert_eq!(text("{{}"), "{}");
    }

    /// Braces that cannot be a placeholder are text rather than an error, so
    /// an argument carrying JSON does not need escaping it has no way to
    /// express.
    #[test]
    fn an_unclosed_brace_is_text() {
        assert_eq!(text("a { b"), "a { b");
    }

    // ─── unset settings ────────────────────────────────────────────────────

    #[test]
    fn an_unset_setting_drops_the_string_rather_than_emptying_it() {
        assert_eq!(filled("{setting:seed}"), Filled::Dropped);
        assert_eq!(filled("seed={setting:seed}"), Filled::Dropped);
    }

    #[test]
    fn the_two_shapes_of_dropping_are_told_apart() {
        assert!(is_sole_placeholder("{setting:seed}"));
        assert!(!is_sole_placeholder("seed={setting:seed}"));
        assert!(!is_sole_placeholder("{setting:a}{setting:b}"));
        assert!(!is_sole_placeholder("-batchmode"));
    }

    #[test]
    fn a_flag_is_a_flag_only_when_it_carries_no_placeholder() {
        assert!(looks_like_flag("+server.seed"));
        assert!(looks_like_flag("-nographics"));
        assert!(!looks_like_flag("{setting:seed}"));
        assert!(!looks_like_flag("-x{port:game}"));
        assert!(!looks_like_flag("28015"));
    }

    // ─── refusals ──────────────────────────────────────────────────────────

    #[test]
    fn an_unknown_placeholder_is_an_error_and_not_an_empty_string() {
        let f = Fixture::new();
        let err = fill("+x {setting}", &f.bindings()).unwrap_err().to_string();
        assert!(err.contains("{setting}"), "names what it found: {err}");

        let err = fill("{waffle}", &f.bindings()).unwrap_err().to_string();
        assert!(err.contains("{waffle}"), "{err}");

        let err = fill("{flavour:x}", &f.bindings()).unwrap_err().to_string();
        assert!(err.contains("{flavour:x}"), "{err}");
    }

    // ─── the address the server is told to bind ────────────────────────────

    #[test]
    fn the_bind_address_resolves_where_a_launch_line_asks_for_it() {
        assert_eq!(text("{bindAddress}"), "127.0.0.1");
        assert_eq!(text("+server.ip {bindAddress}"), "+server.ip 127.0.0.1");
        assert_eq!(
            text("-bind={bindAddress}:{port:game}"),
            "-bind=127.0.0.1:28015"
        );
    }

    /// Player text is never rescanned, and the newest placeholder is no
    /// exception: a server named `{bindAddress}` stays those characters.
    #[test]
    fn a_server_named_after_the_bind_address_is_still_literal_text() {
        let mut f = Fixture::new();
        f.settings.insert("hostname".into(), json!("{bindAddress}"));
        let got = fill("{setting:hostname}", &f.bindings()).unwrap();
        assert_eq!(got.text().unwrap(), "{bindAddress}");
    }

    /// Near-misses are unknown placeholders rather than empty strings, the
    /// same as every other spelling this module does not know.
    #[test]
    fn a_misspelled_bind_address_is_refused_rather_than_ignored() {
        let f = Fixture::new();
        for input in ["{bindaddress}", "{bind_address}", "{bindAddress:game}"] {
            let err = fill(input, &f.bindings()).unwrap_err().to_string();
            assert!(
                err.contains(input.trim_matches(['{', '}'])),
                "{input}: {err}"
            );
        }
    }

    #[test]
    fn a_port_the_server_never_bound_is_an_error() {
        let f = Fixture::new();
        let err = fill("{port:query}", &f.bindings()).unwrap_err().to_string();
        assert!(err.contains("query"), "{err}");
    }

    #[test]
    fn a_secret_that_was_never_generated_is_an_error() {
        let f = Fixture::new();
        let err = fill("{secret:admin}", &f.bindings())
            .unwrap_err()
            .to_string();
        assert!(err.contains("admin"), "{err}");
    }

    /// `{host}` is legal in a join URL, which the API fills in, and is
    /// refused here so a launch line cannot come to depend on an address this
    /// side of the product does not know.
    #[test]
    fn the_gateway_address_cannot_be_used_in_a_launch_line() {
        let f = Fixture::new();
        let err = fill("--advertise={host}", &f.bindings())
            .unwrap_err()
            .to_string();
        assert!(err.contains("{host}"), "{err}");
    }

    #[test]
    fn a_join_url_can_still_be_read_for_its_placeholders() {
        assert_eq!(
            placeholders("steam://connect/{host}:{port:game}").unwrap(),
            vec![Placeholder::Host, Placeholder::Port("game".into())]
        );
    }

    #[test]
    fn every_error_reads_as_a_verdict() {
        let f = Fixture::new();
        for input in ["{waffle}", "{port:nope}", "{secret:nope}", "{host}"] {
            let err = fill(input, &f.bindings()).unwrap_err().to_string();
            for forbidden in ["unwrap", "panicked", "Err(", "None", "errno"] {
                assert!(!err.contains(forbidden), "{input} gave a diagnostic: {err}");
            }
        }
    }
}
