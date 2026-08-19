//! Where a region latency probe is aimed.
//!
//! # Why this is not a URL
//!
//! The API's region list gives each region a `domain`, built from
//! `GatewayHostConfig.public_addr` — "the gateway's public address, the SRV
//! target players' clients resolve", e.g. `us-east.gethomerun.app`. A bare
//! hostname: no scheme, no path, and normally no port, because the port lives
//! in the SRV record rather than in this string.
//!
//! Nothing in the bridge contract says so. `measure-region-latency` is typed
//! `{ params: string; result: number }`, and `string` is the whole of what a
//! host is told. So both mobile hosts guessed, both guessed "URL", and both
//! were wrong in their own way:
//!
//!  - Android called `java.net.URL(domain)`, which requires a protocol. Every
//!    region threw `MalformedURLException`, was swallowed, and came back as
//!    the unreachable sentinel — **without a packet ever being sent**.
//!  - iOS called `URL(string: domain)`, which *succeeds*: a bare hostname is a
//!    valid relative reference. It failed one step later inside `URLSession`
//!    with `unsupportedURL`, reaching the same sentinel by a longer road.
//!
//! Every region reported unreachable, on every device, with nothing logged.
//! The picker ranked a list of ties and took the first, so a player was
//! silently placed in whichever region the API happened to list first.
//!
//! That is the whole argument for this module existing: two platforms
//! answering one question differently, and both answering it wrong.
//!
//! # What this module does not do
//!
//! It opens no socket. Parsing is the part that two platforms can disagree
//! about; the connect is in [`homerun-pumpkin-ffi`'s `host_dispatch`], where
//! the effects live, and there is exactly one of it.
//!
//! [`homerun-pumpkin-ffi`'s `host_dispatch`]: ../../homerun_pumpkin_ffi/host_dispatch/index.html

use serde::{Deserialize, Serialize};

/// The port a region probe knocks on when the address does not name one.
///
/// **The gateway has to actually listen here.** It is tempting to reason that
/// the port is irrelevant because a closed port still resets after one round
/// trip — true on loopback, false on the internet, where a closed port is
/// firewalled and the SYN is *dropped*. Measured against `google.com`,
/// `example.com`, `github.com` and `api.gethomerun.app`, a closed port timed
/// out every time and none refused. So a port nothing serves does not read as
/// slow, it reads as unreachable — for every region at once, which is exactly
/// the symptom this module was written to end.
///
/// 80 is what the desktop has always used (`measure-region-latency` in
/// `ipcHandler.ts`). The three hosts must agree or their numbers stop being
/// comparable, so changing this is a three-host decision.
///
/// # Verified against the real gateways
///
/// A region's address is a per-gateway domain — `minecraft.gethomerun.app`,
/// `redstone.gethomerun.app` — each an explicit DNS-only record pointing at
/// that gateway's VM, and **both serve port 80**. Measured, eight samples
/// each: 39.8 ms and 144.5 ms average, a stable ~105 ms separation. The
/// ranking works.
///
/// The trap is a region with *no* explicit record: `*.gethomerun.app` is a
/// proxied Cloudflare wildcard, so such a name answers at the CDN edge, port
/// 80 open, with a fast plausible number — and sorts better than any real
/// gateway while measuring nothing. See `docs/region-latency.md`.
pub const DEFAULT_PROBE_PORT: u16 = 80;

/// A resolvable host and the TCP port to knock on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeTarget {
    pub host: String,
    pub port: u16,
}

/// Split a region's `domain` into somewhere a socket can be pointed.
///
/// `None` when there is nothing usable to aim at, which the caller reports as
/// unreachable rather than as an error — the UI ranks regions by this number
/// and one bad entry must not cost the whole list.
///
/// # What is tolerated, and what is not
///
/// A `scheme://` prefix is **stripped rather than rejected**, and a trailing
/// path is cut. Mistaking this string for a URL is the confusion that broke
/// the channel once already; a parser that refuses the mistake outright would
/// turn a cosmetic API change into every region going dark.
///
/// A `host:port` form is accepted, because [`crate::link::public_address`]
/// produces exactly that shape for an already-provisioned server and the two
/// should stay interchangeable.
///
/// An explicit port that is **not** a valid one is `None`, not a silent
/// fallback to [`DEFAULT_PROBE_PORT`]. The two failures are not equally
/// costly: an unreachable region is visible in the picker, whereas measuring
/// port 80 while the caller asked for something else yields a plausible number
/// for the wrong thing, and a plausible wrong number is what puts a player on
/// another continent with nobody the wiser.
pub fn probe_target(domain: &str) -> Option<ProbeTarget> {
    let trimmed = domain.trim();

    let after_scheme = match trimmed.find("://") {
        Some(at) => &trimmed[at + 3..],
        None => trimmed,
    };

    // Everything before the first `/`. A bare hostname has no `/` at all, so
    // this is a no-op for the shape actually sent.
    let authority = after_scheme.split('/').next()?;
    if authority.is_empty() {
        return None;
    }

    // A bracketed IPv6 literal carries colons that are not port separators.
    // `public_addr` is a DNS name so this should never arrive, but splitting
    // on the first colon would quietly produce the host `[` and measure
    // nothing — a silent wrong answer, which is the failure mode this whole
    // module exists to avoid.
    let (host, port_text) = match authority.strip_prefix('[') {
        Some(rest) => {
            let (inside, after) = rest.split_once(']')?;
            match after {
                "" => (inside, None),
                more => (inside, Some(more.strip_prefix(':')?)),
            }
        }
        None => match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        },
    };

    if host.is_empty() {
        return None;
    }

    // Port 0 means "any free port" to a bind and nothing at all to a connect.
    let port = match port_text {
        None => DEFAULT_PROBE_PORT,
        Some(text) => text.parse::<u16>().ok().filter(|port| *port != 0)?,
    };

    Some(ProbeTarget {
        host: host.to_string(),
        port,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(host: &str, port: u16) -> Option<ProbeTarget> {
        Some(ProbeTarget {
            host: host.to_string(),
            port,
        })
    }

    /// The shape the API actually sends, and the one both hosts got wrong.
    #[test]
    fn bare_hostname_is_the_normal_case() {
        assert_eq!(
            probe_target("us-east.gethomerun.app"),
            target("us-east.gethomerun.app", DEFAULT_PROBE_PORT)
        );
    }

    #[test]
    fn surrounding_whitespace_is_ignored() {
        assert_eq!(
            probe_target("  us-east.gethomerun.app\n"),
            target("us-east.gethomerun.app", DEFAULT_PROBE_PORT)
        );
    }

    /// `link::public_address` emits this shape; the two must stay compatible.
    #[test]
    fn explicit_port_is_honoured() {
        assert_eq!(
            probe_target("us-east.gethomerun.app:33050"),
            target("us-east.gethomerun.app", 33050)
        );
    }

    #[test]
    fn a_scheme_is_stripped_rather_than_rejected() {
        assert_eq!(
            probe_target("https://us-east.gethomerun.app"),
            target("us-east.gethomerun.app", DEFAULT_PROBE_PORT)
        );
        assert_eq!(
            probe_target("http://us-east.gethomerun.app:8080/status"),
            target("us-east.gethomerun.app", 8080)
        );
    }

    #[test]
    fn a_path_is_cut() {
        assert_eq!(
            probe_target("us-east.gethomerun.app/regions"),
            target("us-east.gethomerun.app", DEFAULT_PROBE_PORT)
        );
    }

    /// The distinction the doc comment argues for: a bad explicit port is
    /// unreachable, never a quiet fallback to 80.
    #[test]
    fn an_unusable_explicit_port_is_refused() {
        assert_eq!(probe_target("us-east.gethomerun.app:notaport"), None);
        assert_eq!(probe_target("us-east.gethomerun.app:99999"), None);
        assert_eq!(probe_target("us-east.gethomerun.app:0"), None);
        assert_eq!(probe_target("us-east.gethomerun.app:-1"), None);
        // Two colons is malformed, not a port of "80:90".
        assert_eq!(probe_target("us-east.gethomerun.app:80:90"), None);
    }

    #[test]
    fn nothing_to_aim_at_is_none() {
        assert_eq!(probe_target(""), None);
        assert_eq!(probe_target("   "), None);
        assert_eq!(probe_target(":80"), None);
        assert_eq!(probe_target("https://"), None);
        assert_eq!(probe_target("/regions"), None);
    }

    /// Not supported, but it must fail honestly rather than measure the host
    /// `[` and report a number.
    #[test]
    fn bracketed_ipv6_does_not_silently_mangle() {
        assert_eq!(probe_target("[::1]"), target("::1", DEFAULT_PROBE_PORT));
        assert_eq!(probe_target("[::1]:25565"), target("::1", 25565));
        assert_eq!(probe_target("[::1"), None);
        // The one thing that must never happen: a host of "[".
        for weird in ["[::1]x", "[]", "[]:80"] {
            let parsed = probe_target(weird);
            assert!(
                parsed.as_ref().is_none_or(|t| t.host != "["),
                "{weird} parsed to {parsed:?}"
            );
        }
    }

    /// A host is never handed on with a port glued to it — that would resolve
    /// as a name and fail, which is the bug in a new costume.
    #[test]
    fn the_host_never_carries_a_port() {
        for input in [
            "us-east.gethomerun.app",
            "us-east.gethomerun.app:33050",
            "https://us-east.gethomerun.app:8080/x",
        ] {
            let parsed = probe_target(input).expect(input);
            assert!(!parsed.host.contains(':'), "{input} -> {parsed:?}");
            assert!(!parsed.host.contains('/'), "{input} -> {parsed:?}");
        }
    }
}
