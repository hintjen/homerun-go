//! Hytale's server sign-in — the effects half.
//!
//! The flow, its reasons and every decision in it are in the pure half,
//! `homerun_core::engine::extensions::hytale`. This file makes the requests:
//!
//! | Hook | Does |
//! |---|---|
//! | `begin` | refresh the kept token (or run the device flow and keep the new one), pick the profile (asking once per server if there are several), open a game session, supply its tokens |
//! | `on_line` | the console fallback: if the server says it is not signed in, send `auth login device` once and pass its sign-in on |
//! | `on_stop` | end the game session |
//! | `forget` / `status` | the kept refresh token |
//!
//! The refresh happens inside [`super::MachineStore::update`], so the lock is
//! held from reading the old token to saving the one that replaced it: two
//! servers starting together cannot both spend the same refresh token, and a
//! crash cannot leave only the spent one on disk.

use std::time::{Duration, Instant};

use homerun_core::engine::descriptor::SignIn as Markers;
use homerun_core::engine::extensions::hytale::{
    self, choose_profile, device_code, profiles, session, session_refusal, stored, stored_refresh,
    text, token_reply, ProfileChoice, TokenReply, DEVICE_GRANT,
};
use homerun_supervisor::fetcher::{replaces_sign_in_url, sign_in_parts};
use serde_json::{json, Value};

use super::{
    Action, Begun, Choice, ExtError, ExtensionStatus, GameExtension, MachineContext, Outcome,
    Prompt, Purpose, Request, Run, SignIn, StartContext, StopContext,
};
use crate::protocol::codes;

pub struct Hytale;

impl GameExtension for Hytale {
    fn name(&self) -> &'static str {
        hytale::SPEC.name
    }

    fn begin(&self, ctx: &mut StartContext) -> Result<Begun, ExtError> {
        let config = ctx.config().clone();
        let access = match refresh(ctx, &config)? {
            Some(access) => access,
            None => sign_in(ctx, &config)?,
        };
        let uuid = profile(ctx, &config, &access)?;
        let session = open_session(ctx, &config, &access, &uuid)?;
        Ok(Begun {
            supplied: [
                ("sessionToken".to_string(), session.session_token.clone()),
                ("identityToken".to_string(), session.identity_token),
                ("ownerUuid".to_string(), uuid),
            ]
            .into(),
            run: Box::new(HytaleRun {
                session_url: text(&config, "sessionUrl").to_string(),
                session_token: session.session_token,
                console: console_markers(&config),
                round: None,
            }),
        })
    }

    fn forget(&self, ctx: &MachineContext) -> Result<(), ExtError> {
        ctx.machine_store().clear()
    }

    fn status(&self, ctx: &MachineContext) -> ExtensionStatus {
        match ctx.machine_store().read() {
            Ok(kept) => ExtensionStatus {
                signed_in: Some(stored_refresh(kept.as_ref()).is_some()),
                account: None,
            },
            Err(_) => ExtensionStatus::default(),
        }
    }
}

// ─── begin ─────────────────────────────────────────────────────────────────

/// Spend the kept refresh token for a new pair, keeping the new refresh token
/// before anything else happens. `None` when there is nothing kept, or what
/// was kept no longer works (then it is forgotten): sign in again.
fn refresh(ctx: &StartContext, config: &Value) -> Result<Option<String>, ExtError> {
    let mut access = None;
    ctx.machine_store().update(|kept| {
        let Some(refresh) = stored_refresh(kept.as_ref()) else {
            return Ok(kept);
        };
        let response = ctx.http(&Request::post(text(config, "tokenUrl")).form(&[
            ("client_id", text(config, "clientId")),
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh),
        ]))?;
        match token_reply(response.status, &response.json().unwrap_or(Value::Null)) {
            TokenReply::Tokens {
                access: new_access,
                refresh: new_refresh,
            } => {
                access = Some(new_access);
                Ok(Some(stored(&new_refresh)))
            }
            TokenReply::InvalidGrant => Ok(None),
            _ => Err(unavailable()),
        }
    })?;
    Ok(access)
}

/// The device flow: show the code, wait while the person approves it, keep
/// the refresh token. Returns the access token.
fn sign_in(ctx: &StartContext, config: &Value) -> Result<String, ExtError> {
    let response = ctx.http(&Request::post(text(config, "deviceUrl")).form(&[
        ("client_id", text(config, "clientId")),
        ("scope", text(config, "scope")),
    ]))?;
    let code = response
        .ok()
        .then(|| response.json())
        .flatten()
        .and_then(|body| device_code(&body))
        .ok_or_else(unavailable)?;
    ctx.sign_in(SignIn {
        purpose: Purpose::Server,
        url: code.url.clone(),
        code: Some(code.user_code.clone()),
        expires_in_secs: Some(code.expires_in),
    })?;
    let deadline = Instant::now() + Duration::from_secs(code.expires_in);
    let mut interval = code.interval;
    loop {
        if ctx.wait(Duration::from_secs(interval)) {
            return Err(ExtError::cancelled());
        }
        if Instant::now() >= deadline {
            return Err(expired());
        }
        let response = ctx.http(&Request::post(text(config, "tokenUrl")).form(&[
            ("client_id", text(config, "clientId")),
            ("grant_type", DEVICE_GRANT),
            ("device_code", &code.device_code),
        ]))?;
        match token_reply(response.status, &response.json().unwrap_or(Value::Null)) {
            TokenReply::Tokens { access, refresh } => {
                ctx.machine_store().update(|_| Ok(Some(stored(&refresh))))?;
                ctx.signed_in(Purpose::Server);
                return Ok(access);
            }
            TokenReply::Pending => {}
            TokenReply::SlowDown => interval += 5,
            TokenReply::Expired => return Err(expired()),
            TokenReply::Denied => {
                return Err(ExtError::new(
                    codes::SIGN_IN_REQUIRED,
                    "The Hytale sign-in was declined, so this server cannot start. Start it \
                     again to sign in.",
                ))
            }
            TokenReply::InvalidGrant | TokenReply::Trouble => return Err(unavailable()),
        }
    }
}

/// The account's profile for this server: remembered, the only one, or asked.
fn profile(ctx: &StartContext, config: &Value, access: &str) -> Result<String, ExtError> {
    let response = ctx.http(&bearer(Request::get(text(config, "profilesUrl")), access))?;
    let list = response
        .ok()
        .then(|| response.json())
        .flatten()
        .and_then(|body| profiles(&body))
        .ok_or_else(unavailable)?;
    let remembered = ctx
        .server_store()
        .read()
        .and_then(|kept| kept["profile"].as_str().map(str::to_string));
    match choose_profile(&list, remembered.as_deref()) {
        ProfileChoice::Use(uuid) => Ok(uuid),
        ProfileChoice::NoProfile => Err(ExtError::new(
            codes::ACCOUNT_NOT_ALLOWED,
            "This Hytale account has no game profile to host a server with.",
        )),
        ProfileChoice::Ask(profiles) => {
            let uuid = ctx.prompt(Prompt {
                title: "Which Hytale profile hosts this server?".into(),
                message: Some("Players will see this server as hosted by that profile.".into()),
                options: profiles
                    .into_iter()
                    .map(|p| Choice {
                        label: if p.username.is_empty() {
                            p.uuid.clone()
                        } else {
                            p.username
                        },
                        value: p.uuid,
                    })
                    .collect(),
            })?;
            ctx.server_store().write(&json!({ "profile": uuid }))?;
            Ok(uuid)
        }
    }
}

fn open_session(
    ctx: &StartContext,
    config: &Value,
    access: &str,
    uuid: &str,
) -> Result<hytale::Session, ExtError> {
    let url = format!("{}/new", text(config, "sessionUrl").trim_end_matches('/'));
    let response = ctx.http(&bearer(Request::post(url), access).json(json!({ "uuid": uuid })))?;
    if let Some(refusal) = session_refusal(response.status, &response.text()) {
        return Err(ExtError::new(codes::ACCOUNT_NOT_ALLOWED, refusal));
    }
    response
        .ok()
        .then(|| response.json())
        .flatten()
        .and_then(|body| session(&body))
        .ok_or_else(unavailable)
}

// ─── while the server runs ─────────────────────────────────────────────────

struct HytaleRun {
    session_url: String,
    session_token: String,
    console: Option<Console>,
    /// The console fallback's current round, if one is open.
    round: Option<Round>,
}

/// The server's own console sign-in: substrings of its lines.
struct Console {
    needed: String,
    command: String,
    markers: Markers,
    done: String,
}

#[derive(Default)]
struct Round {
    url: Option<String>,
    code: Option<String>,
}

fn console_markers(config: &Value) -> Option<Console> {
    let console = config.get("consoleSignIn").filter(|c| c.is_object())?;
    let field = |k: &str| {
        console
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Some(Console {
        needed: field("needed"),
        command: field("command"),
        markers: Markers {
            url: field("url"),
            code: field("code"),
        },
        done: field("done"),
    })
}

impl Run for HytaleRun {
    /// The fallback, for a server that says it is not signed in after all:
    /// its session could not be refreshed while it ran. One round at a time:
    /// the same "not signed in" line repeated does not start a second sign-in
    /// while the first waits on the person.
    fn on_line(&mut self, line: &str, _stream: &str) -> Vec<Action> {
        let Some(console) = &self.console else {
            return vec![];
        };
        let mut actions = vec![];
        if self.round.is_none() && line.contains(&console.needed) {
            self.round = Some(Round::default());
            actions.push(Action::Note(
                "Hytale says this server is not signed in; asking it to sign in.".into(),
            ));
            actions.push(Action::Console(console.command.clone()));
            return actions;
        }
        if self.round.is_some() && line.contains(&console.done) {
            self.round = None;
            actions.push(Action::SignedIn(Purpose::Server));
            return actions;
        }
        let Some(round) = self.round.as_mut() else {
            // Outside a round, a line that looks like a sign-in is not one
            // this server asked for -- it may be a player's chat.
            return actions;
        };
        let (url, code) = sign_in_parts(line, &console.markers);
        let url = url.filter(|new| replaces_sign_in_url(round.url.as_deref(), new));
        let changed = url.is_some() || (code.is_some() && code != round.code);
        if url.is_some() {
            round.url = url;
        }
        if code.is_some() {
            round.code = code;
        }
        if changed {
            if let Some(url) = &round.url {
                actions.push(Action::SignIn(SignIn {
                    purpose: Purpose::Server,
                    url: url.clone(),
                    code: round.code.clone(),
                    expires_in_secs: None,
                }));
            }
        }
        actions
    }

    /// End the game session, so sessions do not pile up towards the
    /// account's limit. A hard kill skips this; the session expires within
    /// the hour on its own.
    fn on_stop(&mut self, ctx: &StopContext, _outcome: Outcome) {
        let url = self.session_url.trim_end_matches('/').to_string();
        match ctx.http(&bearer(Request::delete(url), &self.session_token)) {
            Ok(response) if response.ok() => {}
            _ => ctx.note(
                "Homerun could not end this server's Hytale session; it ends on its own \
                 within the hour.",
            ),
        }
    }
}

// ─── shared ────────────────────────────────────────────────────────────────

fn bearer(request: Request, token: &str) -> Request {
    request.header("Authorization", format!("Bearer {token}"))
}

fn unavailable() -> ExtError {
    ExtError::new(
        codes::VENDOR_UNAVAILABLE,
        "Hytale's sign-in service could not be reached, or did not answer as expected. Try \
         again in a few minutes.",
    )
}

fn expired() -> ExtError {
    ExtError::new(
        codes::SIGN_IN_EXPIRED,
        "The Hytale sign-in code expired before it was used. Start the server again for a \
         new one.",
    )
}

#[cfg(all(test, windows, feature = "test-extensions"))]
mod tests {
    use super::super::harness::{fake_hytale::FakeHytale, Harness};
    use super::super::{Event, Outcome};
    use super::*;

    fn harness(fake: &FakeHytale) -> Harness {
        Harness::for_extension("hytale", fake.config())
    }

    fn sign_ins(h: &Harness) -> Vec<(String, Option<String>)> {
        h.events()
            .into_iter()
            .filter_map(|e| match e {
                Event::SignIn { url, code, .. } => Some((url, code)),
                _ => None,
            })
            .collect()
    }

    fn signed_in(h: &Harness) -> bool {
        h.events().iter().any(|e| {
            matches!(
                e,
                Event::SignedIn {
                    purpose: Purpose::Server,
                    ..
                }
            )
        })
    }

    /// The first start on a machine: the device flow, then a session.
    #[test]
    fn the_first_start_signs_in_keeps_the_refresh_token_and_opens_a_session() {
        let fake = FakeHytale::start();
        let h = harness(&fake);
        let begun = h.begin().unwrap();

        assert_eq!(
            sign_ins(&h),
            vec![(
                "https://accounts.example.com/device?user_code=ABCD-1234".to_string(),
                Some("ABCD-1234".to_string())
            )]
        );
        assert!(signed_in(&h));
        assert_eq!(begun.supplied["sessionToken"], "session-token-1");
        assert_eq!(begun.supplied["identityToken"], "identity-token-1");
        assert_eq!(begun.supplied["ownerUuid"], "uuid-1");
        assert_eq!(
            h.machine().read().unwrap().unwrap()["refreshToken"],
            "refresh-1",
            "the refresh token is kept, sealed"
        );
        assert_eq!(
            fake.calls(),
            vec![
                "POST /oauth2/device/auth",
                "POST /oauth2/token",
                "POST /oauth2/token",
                "GET /my-account/get-profiles",
                "POST /game-session/new",
            ]
        );
        let session = fake.seen().pop().unwrap();
        assert_eq!(session.authorization.as_deref(), Some("Bearer access-1"));
        assert!(session.body.contains("uuid-1"));
    }

    /// Every start after the first spends the kept refresh token and keeps
    /// the one that replaced it; nobody is asked to sign in.
    #[test]
    fn a_later_start_refreshes_and_keeps_the_rotated_token() {
        let fake = FakeHytale::start();
        let h = harness(&fake);
        h.begin().unwrap();
        let before = h.events().len();

        let begun = h.begin().unwrap();
        assert_eq!(h.events().len(), before, "no sign-in the second time");
        assert_eq!(begun.supplied["sessionToken"], "session-token-2");
        assert_eq!(
            h.machine().read().unwrap().unwrap()["refreshToken"],
            "refresh-2"
        );
        let refresh = fake
            .seen()
            .into_iter()
            .rev()
            .find(|s| s.path == "/oauth2/token")
            .unwrap();
        assert!(
            refresh.body.contains("grant_type=refresh_token"),
            "{}",
            refresh.body
        );
        assert!(
            refresh.body.contains("refresh_token=refresh-1"),
            "{}",
            refresh.body
        );
    }

    /// A refresh token that no longer works -- spent elsewhere, revoked, or
    /// 30 days unused -- is forgotten, and the person signs in again.
    #[test]
    fn a_refresh_token_that_no_longer_works_means_signing_in_again() {
        let fake = FakeHytale::start();
        let h = harness(&fake);
        h.begin().unwrap();
        fake.state.lock().unwrap().live_refresh = Some("revoked-elsewhere".into());

        h.begin().unwrap();
        assert_eq!(sign_ins(&h).len(), 2, "signed in again");
        assert_eq!(
            h.machine().read().unwrap().unwrap()["refreshToken"],
            "refresh-2"
        );
    }

    /// An account with several profiles is asked once per server, and the
    /// answer is remembered for that server.
    #[test]
    fn several_profiles_are_asked_about_once_per_server() {
        let fake = FakeHytale::start();
        fake.state.lock().unwrap().profiles = vec![
            ("uuid-a".into(), "Alpha".into()),
            ("uuid-b".into(), "Beta".into()),
        ];
        let h = harness(&fake);
        let answering = h.answer_with("uuid-b");
        let begun = h.begin().unwrap();
        answering.join().unwrap();
        assert_eq!(begun.supplied["ownerUuid"], "uuid-b");
        let options = h.events().into_iter().find_map(|e| match e {
            Event::Prompt { options, .. } => Some(options),
            _ => None,
        });
        assert_eq!(
            options.unwrap(),
            vec![
                Choice {
                    value: "uuid-a".into(),
                    label: "Alpha".into()
                },
                Choice {
                    value: "uuid-b".into(),
                    label: "Beta".into()
                },
            ]
        );

        let prompts = |h: &Harness| {
            h.events()
                .iter()
                .filter(|e| matches!(e, Event::Prompt { .. }))
                .count()
        };
        let asked = prompts(&h);
        // On its own thread: if the answer were not remembered, this start
        // would ask again and wait for an answer nobody gives -- the test
        // should fail, not hang.
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let _ = done.send(h.begin().map(|b| b.supplied["ownerUuid"].clone()));
            });
            let owner = finished
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|_| {
                    h.request_stop();
                    panic!("the second start asked again instead of remembering")
                });
            assert_eq!(owner.unwrap(), "uuid-b");
        });
        assert_eq!(prompts(&h), asked, "not asked again");
    }

    #[test]
    fn a_refused_session_is_explained_and_nothing_is_supplied() {
        let fake = FakeHytale::start();
        fake.state.lock().unwrap().refuse_session = Some("session limit reached".into());
        let error = harness(&fake).begin().err().unwrap();
        assert_eq!(error.code, codes::ACCOUNT_NOT_ALLOWED);
        assert!(
            error.message.contains("as many servers"),
            "{}",
            error.message
        );
    }

    #[test]
    fn an_account_with_no_profile_cannot_host() {
        let fake = FakeHytale::start();
        fake.state.lock().unwrap().profiles.clear();
        let error = harness(&fake).begin().err().unwrap();
        assert_eq!(error.code, codes::ACCOUNT_NOT_ALLOWED);
    }

    #[test]
    fn a_code_nobody_approves_expires_in_words() {
        let fake = FakeHytale::start();
        {
            let mut state = fake.state.lock().unwrap();
            state.never_approve = true;
            state.expires_in = 2;
        }
        // On its own thread: a flow that ignored expiry would poll forever,
        // and this test should fail, not hang.
        let h = harness(&fake);
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let _ = done.send(h.begin().err().map(|e| e.code));
            });
            let code = finished
                .recv_timeout(Duration::from_secs(20))
                .unwrap_or_else(|_| {
                    h.request_stop();
                    panic!("an unapproved code must expire")
                });
            assert_eq!(code, Some(codes::SIGN_IN_EXPIRED));
        });
    }

    /// A player's Stop ends the wait for a sign-in promptly.
    #[test]
    fn a_stop_ends_the_wait_for_a_sign_in() {
        let fake = FakeHytale::start();
        fake.state.lock().unwrap().never_approve = true;
        let h = harness(&fake);
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let _ = done.send(h.begin().is_err());
            });
            while sign_ins(&h).is_empty() {
                std::thread::sleep(Duration::from_millis(20));
            }
            h.request_stop();
            let ended = finished
                .recv_timeout(Duration::from_secs(5))
                .expect("a stop must end the sign-in wait");
            assert!(ended);
        });
        assert!(h.machine().read().unwrap().is_none(), "nothing kept");
    }

    #[test]
    fn an_unreachable_service_fails_the_start_as_unavailable() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut config = FakeHytale::start().config();
        for key in ["deviceUrl", "tokenUrl", "profilesUrl", "sessionUrl"] {
            config[key] = json!(format!("http://127.0.0.1:{port}/x"));
        }
        let error = Harness::for_extension("hytale", config)
            .begin()
            .err()
            .unwrap();
        assert_eq!(error.code, codes::VENDOR_UNAVAILABLE);
    }

    /// At stop the game session is ended with its own token.
    #[test]
    fn the_session_is_ended_at_stop() {
        let fake = FakeHytale::start();
        let h = harness(&fake);
        let mut run = h.begin().unwrap().run;
        h.stop(&mut run, Outcome::Stopped);
        let last = fake.seen().pop().unwrap();
        assert_eq!(
            (last.method.as_str(), last.path.as_str()),
            ("DELETE", "/game-session")
        );
        assert_eq!(
            last.authorization.as_deref(),
            Some("Bearer session-token-1")
        );
    }

    /// The console fallback: one round per "not signed in", the sign-in the
    /// server prints passed on, and closed when the server says it is done.
    #[test]
    fn the_console_fallback_asks_once_and_passes_the_sign_in_on() {
        let fake = FakeHytale::start();
        let h = harness(&fake);
        let mut run = h.begin().unwrap().run;

        // Before any round, a line that looks like a sign-in is ignored: it
        // could be a player's chat.
        assert!(run
            .on_line(
                "Or visit: https://accounts.example.com/device?user_code=X",
                "stdout"
            )
            .is_empty());

        let first = run.on_line("[ServerAuthManager] No server tokens configured.", "stdout");
        assert!(
            first.contains(&Action::Console("auth login device".into())),
            "{first:?}"
        );
        assert!(
            !run.on_line("No server tokens configured.", "stdout")
                .iter()
                .any(|a| matches!(a, Action::Console(_))),
            "one round at a time"
        );
        let shown = run.on_line(
            "\u{1b}[m[INFO] Or visit: https://accounts.example.com/device?user_code=AbCd\u{1b}[m",
            "stdout",
        );
        assert_eq!(
            shown,
            vec![Action::SignIn(SignIn {
                purpose: Purpose::Server,
                url: "https://accounts.example.com/device?user_code=AbCd".into(),
                code: None,
                expires_in_secs: None,
            })]
        );
        // A lookalike host is not passed on, even inside a round.
        assert!(run
            .on_line(
                "Visit https://evil.example/?x=accounts.example.com/device",
                "stdout"
            )
            .is_empty());
        assert_eq!(
            run.on_line("Authentication successful! Mode: OAUTH_DEVICE", "stdout"),
            vec![Action::SignedIn(Purpose::Server)]
        );
        // A later lapse starts a new round.
        assert!(run
            .on_line("No server tokens configured", "stdout")
            .contains(&Action::Console("auth login device".into())));
    }

    #[test]
    fn sign_out_forgets_the_refresh_token() {
        let fake = FakeHytale::start();
        let h = harness(&fake);
        h.begin().unwrap();
        let ctx = h.machine_context();
        assert_eq!(Hytale.status(&ctx).signed_in, Some(true));
        Hytale.forget(&ctx).unwrap();
        assert_eq!(Hytale.status(&ctx).signed_in, Some(false));
        let before = sign_ins(&h).len();
        h.begin().unwrap();
        assert_eq!(
            sign_ins(&h).len(),
            before + 1,
            "signs in again after sign-out"
        );
    }
}
