import Foundation
import UIKit

/// Signing in to a Microsoft account, so a phone can know which Minecraft
/// player it belongs to.
///
/// # Why the phone needs to know
///
/// Minigame stats are keyed on a Minecraft uuid, and every read of them takes
/// one as input. A phone had no way to obtain one, so its Minigames Hub was
/// permanently empty — not broken, just structurally unable to show anybody
/// their own numbers. Most people are covered without any of this, because the
/// API can report an account they linked from the desktop app; this is for
/// somebody whose only device is the phone in their hand, with no Homerun
/// Desktop to link from.
///
/// # Device code, and why not a redirect
///
/// The desktop signs in with the public Xbox client id, whose only registered
/// redirect is a hosted Microsoft page it can watch a `BrowserWindow` navigate
/// to. A phone cannot watch that: intercepting a redirect to a domain we do not
/// own needs a Universal Link we cannot register, and the alternative — an
/// embedded WebView — is the thing Microsoft asks people not to do and takes
/// the user's existing session away from them.
///
/// So this uses the **device code** flow: ask Microsoft for a short code, send
/// the user to their real browser with it already filled in, and poll until
/// they approve. It needs no app registration, no redirect URI, and no
/// Minecraft API approval.
///
/// # Deliberately not ASWebAuthenticationSession
///
/// `auth:web-session` uses one, and this does not, which looks inconsistent
/// until you notice they are different flows. That channel runs a *redirect*
/// and needs to capture a callback URL, which is exactly what
/// `ASWebAuthenticationSession` is for. Device code has no callback: the
/// browser is where the user approves, and the answer comes back to us over a
/// poll on a completely separate connection. Opening it in an auth session
/// would put a "Sign In" consent prompt in front of the user for no benefit,
/// and the sheet would sit there afterwards with nothing to close it.
///
/// # Where the decisions are
///
/// Not here. Every request body, every response shape and every error message
/// is `homerun_core::minecraft::account`, reached through ``Core``, because the
/// chain is five calls deep with a documented trap at nearly every one and
/// Android makes the identical calls. This file opens sockets, sleeps between
/// polls, and writes to the Keychain.
actor MinecraftAuth {

    /// A sign-in that failed for a reason worth showing the player.
    struct AuthError: LocalizedError {
        let message: String
        var errorDescription: String? { message }
    }

    static let shared = MinecraftAuth()

    // MARK: - Storage

    /// The signed-in session, or nil. Tokens included — never leaves this type
    /// except through `Core.accountRedacted`.
    private func stored() -> [String: Any]? {
        guard
            let text = TokenStore.minecraftSession,
            let data = text.data(using: .utf8),
            let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return nil }
        return object
    }

    private func store(_ session: [String: Any]?) {
        guard let session else {
            TokenStore.minecraftSession = nil
            return
        }
        guard
            let data = try? JSONSerialization.data(withJSONObject: session),
            let text = String(data: data, encoding: .utf8)
        else {
            HostLog.bridge.error("could not encode the Minecraft session")
            return
        }
        TokenStore.minecraftSession = text
    }

    // MARK: - The three the bridge asks for

    /// The current account, refreshed if its token has aged out, or nil.
    ///
    /// Nil is an ordinary answer — nobody has signed in — and so is nil after a
    /// refresh that failed, which is a session that cannot be recovered without
    /// the user. The caller reports both the same way: signed out.
    func profile() async -> [String: Any]? {
        guard let session = stored() else { return nil }

        let expiresAt = (session["expiresAt"] as? NSNumber)?.doubleValue ?? 0
        let now = Self.nowMs()
        if let fresh = try? Core.accountNeedsRefresh(expiresAt: expiresAt, nowMs: now), !fresh {
            return session
        }

        guard let refreshToken = session["refreshToken"] as? String, !refreshToken.isEmpty else {
            HostLog.bridge.warning("stored Minecraft session has no refresh token — signing out")
            store(nil)
            return nil
        }

        do {
            // Through the core first: this body is Microsoft's own spelling,
            // unlike a poll outcome, which has already been normalised.
            let refreshed = try Core.accountMsaTokens(
                from: try await exchange(Core.accountRefreshRequest(refreshToken: refreshToken)))
            let session = try await buildSession(from: refreshed)
            store(session)
            return session
        } catch {
            // A refresh can fail because the user revoked access, changed their
            // password, or is simply offline. Only the last is temporary, and
            // this cannot tell them apart — so keep the session and report
            // signed out for now rather than deleting a recoverable login.
            HostLog.bridge.warning(
                "could not refresh the Minecraft session: \(error.localizedDescription, privacy: .public)")
            return nil
        }
    }

    /// Forget the account. Local only — nothing is revoked upstream.
    func signOut() {
        store(nil)
    }

    /// Run an interactive sign-in and return the session once approved.
    ///
    /// Blocks for as long as the code is valid — a quarter of an hour — because
    /// that is how long the user has to finish, and the bridge deliberately has
    /// no call timeout for exactly this kind of operation.
    func signIn() async throws -> [String: Any] {
        let code = try Core.accountDeviceCode(
            from: try await exchangeRaw(Core.accountDeviceCodeRequest()))

        // Opened as soon as there is a code, before the poll that waits up to a
        // quarter of an hour. The code is not sent to the UI, and deliberately:
        // the URL carries it as `?otc=`, so Microsoft fills it in and the user
        // only has to confirm. Telling the page would mean a channel `bridge/v1`
        // does not have, and the whole reason this feature needed no protocol
        // change is that the three invokes and two events already existed.
        await openApproval(code)

        let msa = try await awaitApproval(code)
        let session = try await buildSession(from: msa)
        store(session)
        return session
    }

    /// Open the approval page in the user's own browser.
    ///
    /// Hops to the main actor rather than being declared `@MainActor` — an
    /// actor's own methods cannot be isolated to a different global actor, and
    /// `UIApplication` may only be touched from the main one.
    private func openApproval(_ code: Core.DeviceCode) async {
        // Their real browser, not a WebView we control: it carries whatever
        // Microsoft session they already have, which is usually the difference
        // between approving and typing a password on a phone keyboard.
        guard let url = URL(string: code.approvalURL) else { return }
        await MainActor.run {
            UIApplication.shared.open(url, options: [:]) { opened in
                if !opened {
                    HostLog.bridge.warning("could not open the Microsoft approval page")
                }
            }
        }
    }

    // MARK: - The chain

    /// Poll until the user approves, declines, or the code expires.
    ///
    /// The waiting states arrive as HTTP 400 with an `error` field, which is why
    /// every call here goes through ``exchangeRaw`` and asks the core what the
    /// body meant rather than looking at the status.
    ///
    /// # A failed poll is not a failed sign-in
    ///
    /// This loop runs for up to a quarter of an hour, on a phone, while the
    /// user is in Safari — and iOS is free to suspend this app while they are.
    /// A dropped request in that window is the ordinary case, not the
    /// exceptional one, so a poll that cannot complete is retried. Only three
    /// things end this: an answer from Microsoft, the code expiring, or a
    /// network gone long enough (``giveUpAfterFailures``) that there is no
    /// point pretending otherwise.
    private func awaitApproval(_ code: Core.DeviceCode) async throws -> [String: Any] {
        let request = try Core.accountPollRequest(deviceCode: code.deviceCode)
        let deadline = Date().addingTimeInterval(code.expiresInSecs)
        var interval = code.intervalSecs
        var consecutiveFailures = 0

        while Date() < deadline {
            try await Task.sleep(nanoseconds: UInt64(interval * 1_000_000_000))
            try Task.checkCancellation()

            let outcome: [String: Any]
            do {
                outcome = try Core.accountPollOutcome(try await exchangeRaw(request))
                consecutiveFailures = 0
            } catch is CancellationError {
                throw CancellationError()
            } catch {
                consecutiveFailures += 1
                HostLog.bridge.info(
                    "poll \(consecutiveFailures, privacy: .public) did not get through, still waiting")
                if consecutiveFailures >= Self.giveUpAfterFailures {
                    throw AuthError(
                        message: "Lost the connection while waiting for you to approve the "
                            + "sign-in. Check your connection and try again.")
                }
                continue
            }

            switch outcome["kind"] as? String {
            case "pending":
                break
            // Microsoft asking to be polled less often. Obliging is not
            // optional: keep going at the old rate and it starts refusing.
            case "slowDown":
                interval += 1
            case "declined":
                throw AuthError(message: "Sign-in was declined.")
            case "expired":
                throw AuthError(message: "The sign-in code expired. Please try again.")
            case "approved":
                return outcome
            default:
                throw AuthError(message: "Microsoft returned something unexpected.")
            }
        }
        throw AuthError(message: "The sign-in code expired. Please try again.")
    }

    /// Everything after the Microsoft token: Xbox Live, XSTS, Minecraft, profile.
    ///
    /// Shared by sign-in and refresh, because a refreshed MSA token has to walk
    /// the identical chain — the Minecraft token is not refreshable on its own.
    private func buildSession(from approved: [String: Any]) async throws -> [String: Any] {
        // `approved` is the core's Poll::Approved or an MsaTokens; both carry
        // the same fields, so take them from wherever they landed.
        let msa = (approved["fields"] as? [String: Any]) ?? approved

        let xbl = try Core.accountXboxToken(
            from: try await exchange(
                Core.accountXblRequest(msaAccessToken: try field(msa, "accessToken"))))

        let xstsResponse = try await exchangeRaw(
            Core.accountXstsRequest(xblToken: try field(xbl, "token")))
        // The account-shaped refusals — no Xbox profile, a child account, a
        // region needing verification — all arrive here, and each one names
        // something different for the player to go and fix.
        if let object = xstsResponse as? [String: Any], object["XErr"] != nil {
            throw AuthError(message: try Core.accountXstsRefusal(xstsResponse))
        }
        let xsts = try Core.accountXboxToken(from: xstsResponse)

        let minecraftToken = try Core.accountMinecraftToken(
            from: try await exchange(Core.accountMinecraftLoginRequest(xsts: xsts)))
        let profile = try await exchangeRaw(Core.accountProfileRequest(minecraftToken: minecraftToken))

        return try Core.accountSession(
            profile: profile,
            minecraftToken: minecraftToken,
            msa: msa,
            nowMs: Self.nowMs())
    }

    private func field(_ value: [String: Any], _ key: String) throws -> String {
        guard let text = value[key] as? String else {
            throw AuthError(message: "The sign-in response was missing \"\(key)\".")
        }
        return text
    }

    // MARK: - Transport

    /// Perform a call, failing on a non-2xx.
    private func exchange(_ request: Core.HTTPRequest) async throws -> Any {
        let (status, body) = try await send(request)
        guard (200...299).contains(status) else {
            // Never the body: these responses carry tokens on the way through.
            throw AuthError(message: "Microsoft rejected the sign-in (HTTP \(status)).")
        }
        return body
    }

    /// Perform a call and hand back the body whatever the status was.
    ///
    /// For the three steps where a non-2xx is *information* rather than a
    /// failure: a poll that is still waiting, an XSTS refusal naming an account
    /// restriction, and a profile lookup that 404s because the account does not
    /// own Minecraft. Reading the status alone would turn all three into the
    /// same unhelpful error.
    private func exchangeRaw(_ request: Core.HTTPRequest) async throws -> Any {
        try await send(request).1
    }

    /// Perform a call, retrying a request that never reached Microsoft.
    ///
    /// Only for the transport failing — a reply with a status, however
    /// unwelcome, is an answer and is returned as-is. The retry matters most
    /// *after* approval: by then the user has spent their code, and the four
    /// remaining calls cannot be restarted without sending them back to
    /// Microsoft to approve a second one. Losing a sign-in there to a dropped
    /// packet is the difference between a working feature and one that fails a
    /// few percent of the time for no visible reason.
    private func send(_ request: Core.HTTPRequest) async throws -> (Int, Any) {
        var last: Error?
        for attempt in 0..<Self.transportAttempts {
            do {
                return try await sendOnce(request)
            } catch let error as AuthError {
                // Reaching Microsoft and being told something is an answer.
                throw error
            } catch {
                last = error
                HostLog.bridge.info("request attempt \(attempt + 1, privacy: .public) failed")
                try await Task.sleep(
                    nanoseconds: UInt64(Self.retryBackoff * Double(attempt + 1) * 1_000_000_000))
            }
        }
        // One interpolated literal, not two concatenated: `Logger` takes an
        // `OSLogMessage`, which has no `+`.
        let reason = last?.localizedDescription ?? "no reason given"
        HostLog.bridge.warning("giving up on Microsoft: \(reason, privacy: .public)")
        throw AuthError(message: "Could not reach Microsoft. Check your connection and try again.")
    }

    private func sendOnce(_ request: Core.HTTPRequest) async throws -> (Int, Any) {
        guard let url = URL(string: request.url) else {
            throw AuthError(message: "The sign-in address could not be read.")
        }
        var urlRequest = URLRequest(url: url)
        urlRequest.httpMethod = request.method
        urlRequest.timeoutInterval = Self.requestTimeout
        urlRequest.setValue(Self.userAgent, forHTTPHeaderField: "User-Agent")
        for (name, value) in request.headers {
            urlRequest.setValue(value, forHTTPHeaderField: name)
        }
        if let body = request.body {
            urlRequest.httpBody = body.data(using: .utf8)
        }

        // A URLSession error means the request never landed, which is the one
        // failure worth trying again — it leaves this as a non-AuthError so
        // `send` retries it.
        let (data, response) = try await URLSession.shared.data(for: urlRequest)
        let status = (response as? HTTPURLResponse)?.statusCode ?? 0

        guard !data.isEmpty else {
            throw AuthError(message: "Microsoft returned an empty response (HTTP \(status)).")
        }
        guard let parsed = try? JSONSerialization.jsonObject(with: data) else {
            // Deliberately not the text — an error page is one thing, but this
            // path also sees token responses.
            throw AuthError(message: "Microsoft returned an unreadable response (HTTP \(status)).")
        }
        return (status, parsed)
    }

    private static func nowMs() -> Double {
        Date().timeIntervalSince1970 * 1000
    }

    // MARK: - Numbers

    private static let requestTimeout: TimeInterval = 20
    /// Attempts per request before a network failure is called one.
    private static let transportAttempts = 3
    private static let retryBackoff: TimeInterval = 0.8

    /// Consecutive failed polls before a sign-in gives up.
    ///
    /// Twenty-four at a five-second interval is about two minutes of solid
    /// silence — long enough to ride out a lift, a network handover, or the app
    /// being suspended while the user is in Safari, and short enough that a
    /// genuinely dead connection is not left spinning for the full fifteen.
    private static let giveUpAfterFailures = 24

    private static let userAgent =
        "Homerun-iOS/"
        + ((Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String) ?? "0.0.0")
}
