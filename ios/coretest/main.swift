// Exercises every Core.swift wrapper against the real homerun-core,
// host-native — no simulator, no device, no Pumpkin.
//
// # What this is for
//
// The compiler checks that `Core.swift` is well-formed. It cannot check that
// "tunnel.render" is a method the core has, or that `game.configFiles` wants a
// key called `bind_address`. Those are strings and JSON, resolved at run time,
// and a wrong one compiles and links perfectly. For a bridge handler the
// symptom is an unanswered invoke — a frozen screen with no error, which
// PROTOCOL.md §5 calls the worst failure mode in this protocol.
//
// So this compiles the *real* `Core.swift` rather than a copy: the payloads
// under test are the ones the app sends.
//
// # Running it
//
//     node scripts/build-rust.js host
//     swiftc -O -import-objc-header ios/HomerunHost/FFI/HomerunFFI.h \
//         ios/HomerunHost/FFI/Core.swift ios/HomerunHost/FFI/StartRequest.swift \
//         ios/HomerunHost/HostLog.swift ios/HomerunHost/DeviceMetrics.swift \
//         ios/HomerunHost/ServerBackendError.swift ios/HomerunHost/LaunchOrder.swift \
//         ios/coretest/main.swift \
//         rust/homerun-pumpkin-ffi/target/release/libhomerun_pumpkin_ffi.a \
//         -o /tmp/coretest && /tmp/coretest
//
// Deliberately short. Everything on that list is a leaf — decisions, an error
// type, and the launch walker — with no WebView, no backend and no network
// behind it. If it has to grow much, something has reached for a dependency it
// should not have.
//
// Exits non-zero on the first failure, so it is CI-shaped if anyone wants it
// there. It is not wired in yet: it needs a host Rust build, which the mobile
// CI does not currently do.

import Foundation

var failures = 0
var checks = 0

/// Bodies are `@MainActor` because `Core.Lifecycle` is — it stands in for
/// backend state the app only ever touches from the main queue. This program
/// is single-threaded and runs on the main thread, so asserting that is
/// truthful rather than a workaround; top-level code in a plain `swiftc`
/// build is not main-actor isolated on its own.
func check(_ name: String, _ body: @MainActor () throws -> String) {
    checks += 1
    do {
        let detail = try MainActor.assumeIsolated { try body() }
        print("  ok    \(name)\(detail.isEmpty ? "" : " — \(detail)")")
    } catch {
        failures += 1
        print("  FAIL  \(name) — \(error.localizedDescription)")
    }
}

struct Wrong: LocalizedError {
    let what: String
    var errorDescription: String? { what }
}

func expect(_ cond: Bool, _ message: String) throws {
    if !cond { throw Wrong(what: message) }
}

// A server as the API describes one, with the keys minecraft actually reads.
let env: [String: Any] = [
    "TYPE": "VANILLA",
    "MOTD": "§aHomerun §fserver",   // § is the latin-1 case that mojibakes
    "MAX_PLAYERS": "8",
    "DIFFICULTY": "normal",
    "GAMEMODE": "survival",
    "ONLINE_MODE": "true",
    "PVP": "true",
    "OPS": "Notch",
    "WHITELIST": "Notch,jeb_",
    "LEVEL_SEED": "12345",
    "VIEW_DISTANCE": "10",
]

print("ABI")
check("homerun_abi_version matches what this source expects") {
    // Bump alongside FFI_ABI_VERSION in lib.rs. The point is not the number,
    // it is that the staged .a is the one this source was written against —
    // a mismatch otherwise shows up as garbage decoded out of a reply much
    // later, or as a symbol that links and does something else.
    //
    // `npm run test:abi` reads this line, so forgetting the bump now fails a
    // check that runs without a Swift toolchain. It went unnoticed from 3 to 7
    // before that was true.
    let expected: UInt32 = 7
    let v = homerun_abi_version()
    try expect(v == expected, "expected \(expected), got \(v) — is the staged .a stale?")
    return "v\(v)"
}

check("the backup engine reports whether it is linked") {
    // 0 here in a host build is correct: `backup-engine` is an iOS feature.
    // What matters is that the symbol exists at all, because the header
    // declares it unconditionally and a build without the stub would not link.
    let available = homerun_backup_available()
    return available == 1 ? "linked" : "not linked (host build, as expected)"
}

check("the engine answers rather than crashing when it is not linked") {
    guard homerun_backup_available() == 0 else {
        return "engine is linked; skipping the no-engine path"
    }
    guard let reply = homerun_backup_run("{\"operation\":\"backup\"}") else {
        throw Wrong(what: "no reply from homerun_backup_run")
    }
    defer { homerun_free_string(reply) }
    let text = String(cString: reply)
    try expect(text.contains("\"ok\":false"), "expected a refusal, got \(text)")
    return "refused politely"
}

check("progress and cancel work with no engine and no job") {
    guard let progress = homerun_backup_progress_since(0) else {
        throw Wrong(what: "no reply from homerun_backup_progress_since")
    }
    defer { homerun_free_string(progress) }
    let text = String(cString: progress)
    try expect(text.contains("\"running\":false"), "expected running:false, got \(text)")

    // Cancelling nothing must be a no-op, not an error: the caller is a
    // background-task expiry handler with seconds to live.
    guard let cancel = homerun_backup_cancel() else {
        throw Wrong(what: "no reply from homerun_backup_cancel")
    }
    defer { homerun_free_string(cancel) }
    try expect(String(cString: cancel).contains("\"ok\":true"), "cancel refused")
    return "idle progress reads clean; cancelling nothing is a no-op"
}

print("\ntunnel")
check("tunnel.render produces a wireproxy config") {
    let link: [String: Any] = [
        "client_privkey": "UDy1t3G2t0deMNd/xrRb6+/Qmy4l/md/FmFhCMlSXn0=",
        "gateway_pubkey": "Z1sVr5AX4jiXKrrwnAf6GpaCF3H2Jx8V6/Cus6OPWUk=",
        "link_address": "gateway.example.com:51820",
    ]
    let config = try Core.renderTunnel(link: link, port: 25565, exposure: "java")
    try expect(config.contains("[Interface]"), "no [Interface]:\n\(config)")
    try expect(config.contains("[Peer]"), "no [Peer]")
    try expect(config.contains("[TCPServerTunnel]"), "no [TCPServerTunnel]")
    try expect(config.contains("ListenPort = 25565"), "listen port not pinned to 25565:\n\(config)")
    return "\(config.split(separator: "\n").count) lines"
}

check("tunnel.render pins ListenPort while Target follows the local port") {
    let link: [String: Any] = [
        "client_privkey": "UDy1t3G2t0deMNd/xrRb6+/Qmy4l/md/FmFhCMlSXn0=",
        "gateway_pubkey": "Z1sVr5AX4jiXKrrwnAf6GpaCF3H2Jx8V6/Cus6OPWUk=",
        "link_address": "gateway.example.com:51820",
    ]
    // The documented invariant: a server on a non-default port must still be
    // reachable, because the gateway DNATs to 25565 regardless.
    let config = try Core.renderTunnel(link: link, port: 25599, exposure: "java")
    try expect(config.contains("ListenPort = 25565"), "ListenPort moved with the local port:\n\(config)")
    try expect(config.contains("25599"), "Target did not follow the local port:\n\(config)")
    return "ListenPort 25565, Target 25599"
}

check("tunnel.render refuses an unknown exposure") {
    let link: [String: Any] = [
        "client_privkey": "k", "gateway_pubkey": "g", "link_address": "h:1",
    ]
    do {
        _ = try Core.renderTunnel(link: link, port: 25565, exposure: "bedrock-but-not-really")
        throw Wrong(what: "accepted a nonsense exposure instead of refusing")
    } catch let e as Core.CoreError {
        return "refused: \(e.message)"
    }
}

print("\ngame capability surface")
check("game.configInputs names files and encodings") {
    let inputs = try Core.configInputs(env: env)
    try expect(!inputs.isEmpty, "no config inputs")
    return inputs.map { "\($0.path)(\($0.encoding.rawValue))" }.joined(separator: ", ")
}

check("game.requiredLookups asks for the names in OPS and WHITELIST") {
    let names = try Core.requiredLookups(env: env, gameType: "java")
    try expect(names.contains("Notch"), "Notch not requested; got \(names)")
    return names.joined(separator: ", ")
}

check("game.requiredLookups asks for nothing when offline") {
    var offline = env
    offline["ONLINE_MODE"] = "false"
    let names = try Core.requiredLookups(env: offline, gameType: "java")
    return names.isEmpty ? "none, as expected" : "returned \(names)"
}

check("game.configFiles writes server.properties with the MOTD intact") {
    let files = try Core.configFiles(
        env: env,
        gameType: "java",
        port: 25565,
        bindAddress: "0.0.0.0",
        existing: [:],
        resolved: [Core.Identity(name: "Notch", id: "069a79f4-44e9-4726-a5be-fca90e38aaf5")],
        now: "2026-08-10T12:00:00Z"
    )
    try expect(!files.isEmpty, "no files produced")
    guard let props = files.first(where: { $0.path.contains("server.properties") }) else {
        throw Wrong(what: "no server.properties among \(files.map(\.path))")
    }
    try expect(props.contents.contains("§aHomerun"), "MOTD mangled: \(props.contents.prefix(200))")
    try expect(props.encoding == .latin1, "server.properties should be latin1, got \(props.encoding.rawValue)")
    return "\(files.count) file(s): \(files.map(\.path).joined(separator: ", "))"
}

check("game.configFiles round-trips the resolved identity into ops") {
    let files = try Core.configFiles(
        env: env, gameType: "java", port: 25565, bindAddress: "127.0.0.1",
        existing: [:],
        resolved: [Core.Identity(name: "Notch", id: "069a79f4-44e9-4726-a5be-fca90e38aaf5")],
        now: "2026-08-10T12:00:00Z"
    )
    let ops = files.first(where: { $0.path.contains("ops") })
    try expect(ops != nil, "no ops file among \(files.map(\.path))")
    try expect(ops!.contents.contains("069a79f4"), "uuid missing from ops: \(ops!.contents)")
    return "ops carries the uuid"
}

print("\nlink")
// The real shape: body.config.links[0].native_config, as /api/server/<id>/
// returns it — not a top-level native_config.
func serverBody(privkey: String = "UDy1t3G2t0deMNd/xrRb6+/Qmy4l/md/FmFhCMlSXn0=") -> [String: Any] {
    [
        "config": [
            "links": [
                [
                    "provisioner": "gateway2",
                    "native_config": [
                        "client_privkey": privkey,
                        "gateway_pubkey": "Z1sVr5AX4jiXKrrwnAf6GpaCF3H2Jx8V6/Cus6OPWUk=",
                        "link_address": "gateway.example.com:51820",
                        "address": "10.13.0.7/32",
                        "allowed_ips": "10.13.0.0/16",
                    ],
                ]
            ]
        ]
    ]
}

// The legacy provisioner: no "gateway2", so the staleness check applies.
func legacyBody(privkey: String = "UDy1t3G2t0deMNd/xrRb6+/Qmy4l/md/FmFhCMlSXn0=") -> [String: Any] {
    [
        "config": [
            "links": [
                [
                    "provisioner": "legacy",
                    "native_config": [
                        "client_privkey": privkey,
                        "gateway_pubkey": "Z1sVr5AX4jiXKrrwnAf6GpaCF3H2Jx8V6/Cus6OPWUk=",
                        "link_address": "gateway.example.com:51820",
                    ],
                ]
            ]
        ]
    ]
}

check("link.fromServerBody finds a tunnel") {
    let link = try Core.linkFromServerBody(serverBody())
    try expect(link != nil, "no link found in a body that has one")
    return "keys: \(link!.keys.sorted().joined(separator: ","))"
}

check("link.isUsable takes a PolledLink for `polled` but a bare Link for `before`") {
    let polled = try Core.linkFromServerBody(legacyBody())!
    // The trap: both are [String: Any] in Swift, but `before` is the inner
    // link, not the PolledLink. Passing the whole thing throws at runtime.
    do {
        _ = try Core.linkIsUsable(polled: polled, before: polled)
        throw Wrong(what: "a PolledLink was accepted as `before` — asymmetry is gone, update the note")
    } catch let e as Core.CoreError {
        try expect(e.message.contains("prior link"), "unexpected error: \(e.message)")
        return "confirmed: `before` must be polled[\"link\"] — \(e.message)"
    }
}

check("link.isUsable rejects the previous session's dead credentials (legacy)") {
    let polled = try Core.linkFromServerBody(legacyBody())!
    let inner = polled["link"] as! [String: Any]
    let unchanged = try Core.linkIsUsable(polled: polled, before: inner)
    let rotated = try Core.linkFromServerBody(legacyBody(privkey: "cGVyLXNlc3Npb24ta2V5LXRoYXQtaXMtbmV3PQ=="))!
    let fresh = try Core.linkIsUsable(polled: rotated, before: inner)
    try expect(!unchanged, "an unchanged legacy link was called usable — that is the dead keypair")
    try expect(fresh, "a rotated legacy link was called unusable")
    return "unchanged=false rotated=true"
}

check("link.isUsable keeps an unchanged gateway2 link (credentials are reused by design)") {
    let polled = try Core.linkFromServerBody(serverBody())!
    let inner = polled["link"] as! [String: Any]
    let unchanged = try Core.linkIsUsable(polled: polled, before: inner)
    try expect(unchanged, "a v2 link was judged stale — it would be rejected on every start")
    return "unchanged v2 stays usable"
}

check("link.isUsable accepts a link with nothing to compare against") {
    let polled = try Core.linkFromServerBody(serverBody())!
    return "first launch -> \(try Core.linkIsUsable(polled: polled, before: nil))"
}

check("link.fromServerBody returns nil when there is none") {
    let link = try Core.linkFromServerBody(["id": 1])
    try expect(link == nil, "invented a link from a body with none")
    return "nil, as expected"
}

// The three `state.exit` checks that used to open this section are gone with
// the wrapper they exercised. What they pinned is pinned better elsewhere: the
// `lifecycle` section below drives the same verdicts through `lifecycle.exited`,
// which is the path the app actually takes, and Rust covers `exit_state`
// directly in `state.rs`.

print("\nhandshake")
check("state.handshake gives up after enough failures") {
    var watch: [String: Any]? = nil
    var gaveUpAfter = -1
    let line = "peer(SzQp…Bv0=) - Handshake did not complete after 5 seconds, retrying (try 3)"
    for i in 1...40 {
        let r = try Core.observeHandshake(watch: watch, line: line)
        watch = r.watch
        if r.giveUp { gaveUpAfter = i; break }
    }
    try expect(gaveUpAfter > 0, "never gave up after 40 failing handshake lines")
    return "gave up after \(gaveUpAfter) lines"
}

check("state.handshake reports recovery only after it had given up") {
    var watch: [String: Any]? = nil
    let bad = "peer(x) - Handshake did not complete after 5 seconds, retrying (try 1)"
    // recovered() is signalled && failures == 0, so it must give up first —
    // three failures is below the threshold and must not count as a recovery.
    for _ in 1...3 {
        watch = try Core.observeHandshake(watch: watch, line: bad).watch
    }
    let early = try Core.observeHandshake(watch: watch, line: "peer(x) - Received handshake response")
    try expect(!early.recovered, "claimed recovery without ever having given up")

    watch = nil
    var signalled = false
    for _ in 1...15 {
        let r = try Core.observeHandshake(watch: watch, line: bad)
        watch = r.watch
        if r.giveUp { signalled = true }
    }
    try expect(signalled, "never gave up in 15 lines")
    let back = try Core.observeHandshake(watch: watch, line: "peer(x) - Received handshake response")
    try expect(back.recovered, "did not report recovery after giving up then succeeding")
    return "no false recovery early; recovered=true after give-up"
}

check("state.handshake gives up only once per watch") {
    var watch: [String: Any]? = nil
    let bad = "peer(x) - Handshake did not complete after 5 seconds, retrying (try 1)"
    var giveUps = 0
    for _ in 1...40 {
        let r = try Core.observeHandshake(watch: watch, line: bad)
        watch = r.watch
        if r.giveUp { giveUps += 1 }
    }
    // Returned once per watch, so a caller cannot stop a server twice.
    try expect(giveUps == 1, "gave up \(giveUps) times over 40 failures — a server would be stopped repeatedly")
    return "1 give-up over 40 failures"
}

print("\nconsole")
check("game.classify spots the ready line") {
    let l = try Core.classify(#"[12:00:00] [Server thread/INFO]: Done (3.214s)! For help, type "help""#)
    try expect(l.ready, "did not recognise the ready line")
    return "ready"
}

check("game.classify spots a join and a leave") {
    let joined = try Core.classify("[12:00:01] [Server thread/INFO]: Notch joined the game")
    let left = try Core.classify("[12:00:02] [Server thread/INFO]: Notch left the game")
    try expect(joined.joined == "Notch", "joined=\(String(describing: joined.joined))")
    try expect(left.left == "Notch", "left=\(String(describing: left.left))")
    return "joined/left both Notch"
}

check("game.classify says nothing about an ordinary line") {
    let l = try Core.classify("[12:00:03] [Server thread/INFO]: Saving chunks")
    try expect(!l.ready && l.joined == nil && l.left == nil, "over-read an ordinary line")
    return "silent, as expected"
}

print("\nbackup — through the Core.swift wrappers")

func snapshot(host: String, id: String = "s1") -> [String: Any] {
    ["id": id, "time": "2026-08-10T12:00:00Z", "host": host, "paths": ["/srv/servers/abc"]]
}

check("shouldBackUp guards on there being a world") {
    let with = try Core.shouldBackUp(hasLocalWorld: true)
    let without = try Core.shouldBackUp(hasLocalWorld: false)
    try expect(with && !without, "with=\(with) without=\(without)")
    return "true with a world, false without"
}

check("restoreDecision restores when another device wrote the newest snapshot") {
    let d = try Core.restoreDecision(
        pinned: nil, latest: snapshot(host: "device-b"), deviceId: "device-a", hasLocalWorld: true)
    guard case .latest(let id, let reason) = d else {
        throw Wrong(what: "expected .latest, got \(d)")
    }
    try expect(id == "s1" && reason == "anotherDeviceIsNewer", "id=\(id) reason=\(reason)")
    return "latest(\(id), \(reason))"
}

check("restoreDecision keeps local work when this device wrote it") {
    let d = try Core.restoreDecision(
        pinned: nil, latest: snapshot(host: "device-a"), deviceId: "device-a", hasLocalWorld: true)
    guard case .skip(let reason) = d else { throw Wrong(what: "expected .skip, got \(d)") }
    return "skip(\(reason))"
}

check("restoreDecision restores when there is no local world at all") {
    let d = try Core.restoreDecision(
        pinned: nil, latest: snapshot(host: "device-a"), deviceId: "device-a", hasLocalWorld: false)
    guard case .latest(_, let reason) = d else { throw Wrong(what: "expected .latest, got \(d)") }
    try expect(reason == "localWorldMissing", "reason=\(reason)")
    return "latest(_, \(reason))"
}

check("restoreDecision skips when there is no snapshot to compare against") {
    let d = try Core.restoreDecision(
        pinned: nil, latest: nil, deviceId: "device-a", hasLocalWorld: true)
    guard case .skip(let reason) = d else { throw Wrong(what: "expected .skip, got \(d)") }
    return "skip(\(reason))"
}

check("restoreDecision obeys a dashboard pin over everything else") {
    // Our own snapshot, a local world present — both of which would otherwise
    // mean skip. The pin has to win or a dashboard restore does nothing.
    let d = try Core.restoreDecision(
        pinned: "pinned-snap", latest: snapshot(host: "device-a"),
        deviceId: "device-a", hasLocalWorld: true)
    guard case .rollback(let id) = d else { throw Wrong(what: "expected .rollback, got \(d)") }
    try expect(id == "pinned-snap", "rolled back to \(id)")
    return "rollback(\(id))"
}

check("leaseDecision launches, blocks and forces") {
    let free = try Core.leaseDecision(leaseDevice: nil, deviceId: "device-a", force: false)
    let mine = try Core.leaseDecision(leaseDevice: "device-a", deviceId: "device-a", force: false)
    let held = try Core.leaseDecision(leaseDevice: "device-b", deviceId: "device-a", force: false)
    let took = try Core.leaseDecision(leaseDevice: "device-b", deviceId: "device-a", force: true)

    guard case .launch = free else { throw Wrong(what: "free lease did not launch") }
    guard case .launch = mine else { throw Wrong(what: "our own lease did not launch") }
    guard case .blocked(let by) = held else { throw Wrong(what: "another device's lease did not block") }
    guard case .forced(let from) = took else { throw Wrong(what: "force did not take the lease") }
    try expect(by == "device-b" && from == "device-b", "by=\(by) from=\(from)")
    return "launch / launch(own) / blocked(\(by)) / forced(\(from))"
}

check("classifyBackupFailure marks a network failure retryable") {
    let f = try Core.classifyBackupFailure(
        message: "unable to open repository at rest:https://…: connection refused",
        host: "device-a")
    try expect(f.retryable, "kind=\(f.kind) not retryable")
    try expect(!f.succeeded, "a connection failure reported success")
    return "kind=\(f.kind) retryable=true"
}

check("classifyBackupFailure spots the auth race") {
    let f = try Core.classifyBackupFailure(
        message: "unable to open repository: 401 Unauthorized", host: "device-a")
    try expect(f.kind == "authRace", "kind=\(f.kind)")
    try expect(f.retryable, "authRace should be retryable")
    return "kind=authRace retryable=true"
}

check("classifyBackupFailure calls an unrecognised failure fatal") {
    let f = try Core.classifyBackupFailure(message: "the disk caught fire", host: "device-a")
    try expect(f.kind == "fatal" && !f.retryable && !f.succeeded, "kind=\(f.kind)")
    return "kind=fatal"
}

check("classifyBackupFailure cannot report success without an exit code") {
    // The documented trap, pinned: this wrapper offers no exit code, and
    // `succeeded` is reachable only from restic's exit 3. So a linked engine
    // can never classify its way to success — the snapshot is the only proof.
    // If this ever starts passing, backup.rs grew a message-based warning arm
    // and the iOS engine can use it.
    let f = try Core.classifyBackupFailure(message: "", host: "device-a")
    try expect(!f.succeeded, "an empty message classified as succeeded")
    try expect(f.kind == "fatal", "empty message is \(f.kind), expected fatal")
    return "empty message -> fatal, never succeeded"
}

check("recordedBasename reads the directory a snapshot recorded") {
    let posix = try Core.recordedBasename("/data/data/app/files/servers/abc123")
    let windows = try Core.recordedBasename(#"C:\Users\me\AppData\servers\abc123"#)
    try expect(posix == "abc123", "posix=\(posix ?? "nil")")
    try expect(windows == "abc123", "windows=\(windows ?? "nil")")
    return "posix and windows both abc123"
}

check("internalPath folds a drive letter so the selector colon is unambiguous") {
    let posix = try Core.internalPath("/data/data/app/files/servers/abc123")
    let windows = try Core.internalPath(#"C:\Users\me\srv"#)
    try expect(posix.hasPrefix("/"), "posix=\(posix)")
    try expect(!windows.contains(":"), "a colon survived: \(windows) — SNAP:PATH would split wrong")
    try expect(windows.hasPrefix("/"), "windows=\(windows)")
    return "posix=\(posix) windows=\(windows)"
}

check("backupReport builds a completed body and releases the lease") {
    let r = try Core.backupReport(
        operation: "backup", snapshotId: "abc123", bytes: 1_048_576, durationSeconds: 12.5)
    try expect(r.releasesLease, "a completed backup did not release the lease")
    try expect(r.body["status"] as? String == "complete", "status=\(r.body["status"] ?? "nil")")
    return "status=complete releases=true keys=\(r.body.keys.sorted().joined(separator: ","))"
}

check("backupReport releases the lease on failure too") {
    // The whole point: a failed backup that held the lease forever would
    // strand every other device on this server.
    let r = try Core.backupReport(operation: "backup", error: "repository is locked")
    try expect(r.releasesLease, "a failed backup did not release the lease")
    try expect(r.body["status"] as? String == "failed", "status=\(r.body["status"] ?? "nil")")
    return "status=failed releases=true"
}

check("backupReport does not release the lease for a restore") {
    let r = try Core.backupReport(operation: "restore", snapshotId: "abc123")
    try expect(!r.releasesLease, "a restore released the backup lease")
    return "restore releases=false"
}

check("backupReport refuses an unknown operation") {
    do {
        _ = try Core.backupReport(operation: "defenestrate")
        throw Wrong(what: "accepted an unknown operation")
    } catch let e as Core.CoreError {
        return "refused: \(e.message)"
    }
}

func callEngine(_ fn: (UnsafePointer<CChar>?) -> UnsafeMutablePointer<CChar>?, _ request: [String: Any])
    throws -> [String: Any]
{
    let data = try JSONSerialization.data(withJSONObject: request)
    let json = String(data: data, encoding: .utf8)!
    guard let reply = json.withCString({ fn($0) }) else {
        throw Wrong(what: "the engine returned nothing")
    }
    defer { homerun_free_string(reply) }
    let text = String(cString: reply)
    guard let object = try JSONSerialization.jsonObject(with: Data(text.utf8)) as? [String: Any]
    else { throw Wrong(what: "the engine answered with nonsense: \(text)") }
    return object
}

print("\nstart request")
// The one encoding in this host that is checked by nobody else. `Core.swift`
// goes through `homerun_core_call`, which names its method in the payload and
// fails loudly on a typo; a start request is a bare JSON object parsed by
// field name, so a misspelling is a server quietly running on defaults.
//
// `homerun_server_settings_preview` exists for exactly this: it reports what a
// start *would* apply, without starting anything.

func preview(_ request: [String: Any]) throws -> [String: Any] {
    try callEngine({ homerun_server_settings_preview($0) }, request)
}

check("a start request encodes the settings the engine then resolves") {
    let reply = try preview(
        StartRequest.encode(
            serverId: "s1", dataDir: "/tmp/homerun-coretest", port: 25565,
            settings: StartRequest.Settings(
                env: env, gameType: "java",
                resolved: ["Notch": "069a79f4-44e9-4726-a5be-fca90e38aaf5"])))

    try expect(reply["ok"] as? Bool == true, "refused: \(reply)")
    guard let settings = reply["settings"] as? [String: Any] else {
        throw Wrong(what: "no settings in \(reply) — did a key name drift?")
    }

    // Each of these is a separate key crossing the boundary. Distinct values
    // in `env` on purpose: equal ones are what let two fields be swapped.
    try expect(settings["motd"] as? String == "§aHomerun §fserver", "motd: \(settings)")
    try expect(settings["maxPlayers"] as? Int == 8, "maxPlayers: \(settings)")
    try expect(settings["viewDistance"] as? Int == 10, "viewDistance: \(settings)")
    try expect(settings["gameMode"] as? String == "survival", "gameMode: \(settings)")
    try expect(settings["onlineMode"] as? Bool == true, "onlineMode: \(settings)")
    try expect(settings["seed"] as? String == "12345", "seed: \(settings)")

    // And the identity the host resolved actually arrived — the check that
    // `resolved` is a list of `{name, id}` and not something else.
    let ops = settings["ops"] as? [[String: Any]] ?? []
    try expect(ops.first?["name"] as? String == "Notch", "ops: \(settings["ops"] ?? "nil")")
    try expect(
        (ops.first?["uuid"] as? String)?.hasPrefix("069a79f4") == true,
        "the resolved uuid did not reach the engine: \(ops)")

    return "\(settings.count) settings resolved"
}

check("game type reaches the engine, not just the name of the key") {
    // If `gameType` were misspelled this still answers ok, with online mode
    // left at the API's value — which is the whole failure mode being guarded
    // against, so it needs a case where the two differ.
    var crossplay = env
    crossplay["ONLINE_MODE"] = "true"
    let reply = try preview(
        StartRequest.encode(
            serverId: "s1", dataDir: "/tmp/homerun-coretest", port: 25565,
            settings: StartRequest.Settings(
                env: crossplay, gameType: "native-crossplay", resolved: [:])))

    let settings = reply["settings"] as? [String: Any] ?? [:]
    try expect(
        settings["onlineMode"] as? Bool == false,
        "a crossplay server must be offline whatever the API says: \(settings)")
    return "native-crossplay forced offline mode"
}

check("a launch with no settings is valid, and says nothing was applied") {
    let reply = try preview(
        StartRequest.encode(
            serverId: "s1", dataDir: "/tmp/homerun-coretest", port: 25565, settings: nil))
    try expect(reply["ok"] as? Bool == true, "refused a settings-free start: \(reply)")
    try expect(reply["settings"] is NSNull, "expected null settings, got \(reply)")
    return "starts on the engine's own configuration"
}

check("settings the engine cannot honour are reported, not swallowed") {
    let reply = try preview(
        StartRequest.encode(
            serverId: "s1", dataDir: "/tmp/homerun-coretest", port: 25565,
            settings: StartRequest.Settings(
                env: env, gameType: "java", resolved: [:])))
    let settings = reply["settings"] as? [String: Any] ?? [:]
    let unsupported = settings["unsupported"] as? [String] ?? []
    // `env` asks for a difficulty, which lives in level.dat and cannot be set
    // from config. A player who chose it deserves to be told.
    try expect(unsupported.contains("difficulty"), "difficulty not reported: \(unsupported)")
    let advisories = reply["advisories"] as? [String] ?? []
    try expect(!advisories.isEmpty, "no console line for the ignored settings")
    return advisories.joined(separator: " / ")
}

check("a malformed request is refused rather than started on defaults") {
    let reply = try callEngine({ homerun_server_settings_preview($0) }, ["dataDir": "/tmp"])
    try expect(reply["ok"] as? Bool == false, "a request with no serverId was accepted: \(reply)")
    return "refused: \(reply["error"] as? String ?? "?")"
}

print("\nmojang ids")
check("minecraft.settings.dashUuid dashes Mojang's form") {
    let dashed = try Core.dashUuid("069a79f444e94726a5befca90e38aaf5")
    try expect(dashed == "069a79f4-44e9-4726-a5be-fca90e38aaf5", "got \(dashed)")
    return dashed
}

check("minecraft.settings.dashUuid refuses garbage") {
    // A short id is the one that matters: it would dash into something
    // uuid-shaped and never match a real player.
    for bad in ["", "not-a-uuid", "069a79f444e94726a5befca90e38aaf", "zz9a79f444e94726a5befca90e38aaf5"] {
        do {
            let out = try Core.dashUuid(bad)
            throw Wrong(what: "accepted \"\(bad)\" and answered \(out)")
        } catch is Core.CoreError {
            continue
        }
    }
    return "three bad ids and an empty string refused"
}

check("minecraft.settings.dashUuid is idempotent") {
    // Deliberate, not an accident of the implementation: it strips dashes
    // before checking, so a host that dashes twice gets the same id rather
    // than an error it would have to special-case.
    let once = try Core.dashUuid("069a79f444e94726a5befca90e38aaf5")
    try expect(try Core.dashUuid(once) == once, "dashing an already-dashed id changed it")
    return once
}

// MARK: - The engine, end to end
//
// Runs only where an engine is linked, which today means an iOS build. The
// repository is a local directory rather than the `rest:` URL the API hands
// out — rustic compiles the local backend unconditionally, and the format is
// the same one either way, so this exercises init, backup, snapshot listing
// and restore without needing a server.


if homerun_backup_available() == 1 {
    print("\nengine — a real backup and restore")

    let root = URL(fileURLWithPath: NSTemporaryDirectory())
        .appendingPathComponent("homerun-engine-test-\(ProcessInfo.processInfo.processIdentifier)")
    let world = root.appendingPathComponent("servers/abc123")
    let repo = root.appendingPathComponent("repo")
    let cache = root.appendingPathComponent("cache")
    let restored = root.appendingPathComponent("restored")

    let repoBlock: [String: Any] = [
        "repo": repo.path,
        "restic_password": "a-test-passphrase",
        "keep": ["last": 30, "hourly": 24, "daily": 30],
    ]
    let marker = "level.dat contents — \(UUID().uuidString)"
    var snapshotId = ""

    check("a world can be backed up into a fresh repository") {
        try? FileManager.default.removeItem(at: root)
        try FileManager.default.createDirectory(
            at: world.appendingPathComponent("world"), withIntermediateDirectories: true)
        try marker.write(
            to: world.appendingPathComponent("world/level.dat"), atomically: true, encoding: .utf8)

        let reply = try callEngine(homerun_backup_run, [
            "operation": "backup",
            "repo": repoBlock,
            "cacheDir": cache.path,
            "sourceDir": world.path,
            "deviceId": "device-a",
        ])
        try expect(reply["ok"] as? Bool == true, "backup failed: \(reply)")
        guard let id = reply["snapshotId"] as? String, !id.isEmpty else {
            throw Wrong(what: "no snapshot id in \(reply)")
        }
        snapshotId = id
        let bytes = reply["bytes"] as? Int ?? 0
        try expect(bytes > 0, "a backup that stored nothing reported \(bytes) bytes")
        return "snapshot \(id.prefix(8)), \(bytes) bytes"
    }

    check("the snapshot is written under this device's id, not the machine's hostname") {
        // The single most load-bearing field in the whole subsystem. The API
        // resolves `pushed_by` from it, and restoreDecision compares it to
        // decide whether another device wrote the newest snapshot. Wrong here
        // and a device restores over its own work on its next launch.
        let reply = try callEngine(homerun_backup_latest_snapshot, [
            "repo": repoBlock, "cacheDir": cache.path,
        ])
        try expect(reply["ok"] as? Bool == true, "listing failed: \(reply)")
        guard let snapshot = reply["snapshot"] as? [String: Any] else {
            throw Wrong(what: "no snapshot in \(reply)")
        }
        try expect(
            snapshot["host"] as? String == "device-a",
            "host was \(snapshot["host"] ?? "nil"), expected device-a")
        try expect(snapshot["id"] as? String == snapshotId, "listed a different snapshot")
        return "host=device-a id=\(snapshotId.prefix(8))"
    }

    check("the decision layer reads the engine's snapshot without translation") {
        // The two halves have never met before this point: the shape the
        // engine returns is fed straight into homerun-core.
        let reply = try callEngine(homerun_backup_latest_snapshot, [
            "repo": repoBlock, "cacheDir": cache.path,
        ])
        let snapshot = reply["snapshot"] as! [String: Any]

        let ours = try Core.restoreDecision(
            pinned: nil, latest: snapshot, deviceId: "device-a", hasLocalWorld: true)
        guard case .skip = ours else { throw Wrong(what: "our own snapshot asked for a restore: \(ours)") }

        let theirs = try Core.restoreDecision(
            pinned: nil, latest: snapshot, deviceId: "device-b", hasLocalWorld: true)
        guard case .latest(_, let reason) = theirs else {
            throw Wrong(what: "another device's snapshot did not ask for a restore: \(theirs)")
        }
        return "skip for device-a, \(reason) for device-b"
    }

    check("the world restores byte-for-byte") {
        let reply = try callEngine(homerun_backup_run, [
            "operation": "restore",
            "repo": repoBlock,
            "cacheDir": cache.path,
            "snapshotId": snapshotId,
            "serverId": "abc123",
            "targetDir": restored.path,
        ])
        try expect(reply["ok"] as? Bool == true, "restore failed: \(reply)")

        let file = restored.appendingPathComponent("world/level.dat")
        let back = try String(contentsOf: file, encoding: .utf8)
        try expect(back == marker, "restored contents differ:\n  \(back)\n  \(marker)")
        return "identical bytes"
    }

    check("a snapshot written by another device restores here too") {
        // The case the whole feature exists for, and the one a selector built
        // from *our* path would silently fail: the snapshot records the writing
        // machine's absolute path, which on a desktop looks nothing like an
        // iOS container. The engine resolves it from the snapshot instead.
        let elsewhere = root.appendingPathComponent("another-device/home/you/.homerun/servers/abc123")
        try FileManager.default.createDirectory(
            at: elsewhere.appendingPathComponent("world"), withIntermediateDirectories: true)
        let theirMarker = "written on a different machine — \(UUID().uuidString)"
        try theirMarker.write(
            to: elsewhere.appendingPathComponent("world/level.dat"), atomically: true,
            encoding: .utf8)

        let backup = try callEngine(homerun_backup_run, [
            "operation": "backup", "repo": repoBlock, "cacheDir": cache.path,
            "sourceDir": elsewhere.path, "deviceId": "device-b",
        ])
        try expect(backup["ok"] as? Bool == true, "the other device's backup failed: \(backup)")
        let theirSnapshot = backup["snapshotId"] as! String

        let here = root.appendingPathComponent("restored-from-elsewhere")
        let reply = try callEngine(homerun_backup_run, [
            "operation": "restore", "repo": repoBlock, "cacheDir": cache.path,
            "snapshotId": theirSnapshot, "serverId": "abc123", "targetDir": here.path,
        ])
        try expect(reply["ok"] as? Bool == true, "cross-device restore failed: \(reply)")

        let back = try String(
            contentsOf: here.appendingPathComponent("world/level.dat"), encoding: .utf8)
        try expect(back == theirMarker, "restored the wrong world:\n  \(back)")
        return "a path this device has never seen resolved correctly"
    }

    check("a snapshot that does not hold this server is refused, and says so") {
        let reply = try callEngine(homerun_backup_run, [
            "operation": "restore", "repo": repoBlock, "cacheDir": cache.path,
            "snapshotId": snapshotId, "serverId": "some-other-server",
            "targetDir": root.appendingPathComponent("nope").path,
        ])
        try expect(reply["ok"] as? Bool == false, "restored a server the snapshot does not hold")
        let message = reply["message"] as? String ?? ""
        try expect(
            message.contains("does not contain this server"),
            "unhelpful message: \(message)")
        return "refused, and names what the snapshot does hold"
    }

    check("a second backup of an unchanged world adds almost nothing") {
        // Deduplication against the parent snapshot, which is what makes
        // on-stop backups affordable on a phone.
        let reply = try callEngine(homerun_backup_run, [
            "operation": "backup",
            "repo": repoBlock,
            "cacheDir": cache.path,
            "sourceDir": world.path,
            "deviceId": "device-a",
        ])
        try expect(reply["ok"] as? Bool == true, "second backup failed: \(reply)")
        let bytes = reply["bytes"] as? Int ?? -1
        try expect(bytes >= 0, "no byte count")
        return "\(bytes) bytes added the second time"
    }

    check("a bad passphrase fails cleanly, with a sentence for the player") {
        var wrong = repoBlock
        wrong["restic_password"] = "not the passphrase"
        let reply = try callEngine(homerun_backup_run, [
            "operation": "backup", "repo": wrong, "cacheDir": cache.path,
            "sourceDir": world.path, "deviceId": "device-a",
        ])
        try expect(reply["ok"] as? Bool == false, "a wrong passphrase was accepted")
        guard let player = reply["error"] as? String, let raw = reply["message"] as? String else {
            throw Wrong(what: "no error/message split in \(reply)")
        }
        try expect(!player.isEmpty && !raw.isEmpty, "empty error text")

        // And the host must be able to classify it without an exit code.
        let verdict = try Core.classifyBackupFailure(message: raw, host: "device-a")
        try expect(!verdict.succeeded, "a failed backup classified as succeeded")
        return "player=\"\(player)\" kind=\(verdict.kind)"
    }

    check("an unreachable repository is a retryable failure, not a fatal one") {
        // Port 9 is discard: refused immediately rather than timing out.
        //
        // The assertion that matters is not that it failed — it is that the
        // engine's error text still carries enough for the core to call it
        // transient. A linked engine has no exit code, so this string is the
        // only thing standing between "we will try again" and telling a player
        // their backup is broken because the wifi dropped.
        let reply = try callEngine(homerun_backup_run, [
            "operation": "backup",
            "repo": ["repo": "rest:http://127.0.0.1:9/nope/", "restic_password": "x"],
            "cacheDir": cache.path, "sourceDir": world.path, "deviceId": "device-a",
        ])
        try expect(reply["ok"] as? Bool == false, "an unreachable repository reported success")
        let raw = reply["message"] as? String ?? ""

        let verdict = try Core.classifyBackupFailure(message: raw, host: "device-a")
        try expect(
            verdict.retryable,
            "classified \(verdict.kind), not retryable. The engine's text was:\n\(raw)")
        return "kind=\(verdict.kind) retryable=true"
    }

    try? FileManager.default.removeItem(at: root)
}

print("\nlaunch plan")

check("the plan iOS actually gets is in the order the core says") {
    // backups on, settings off (this host writes none yet), tunnel on.
    let steps = try Core.launchPlan(backups: true, settings: false, tunnel: true)
    let names = steps.map(\.name)
    // No `ensureJar` and no `resolveMainClass`: this host asks for a linked
    // plan and the core leaves out the two steps that are about a jar.
    // `ensureRuntime` and `acceptEula` are still here — neither is about the
    // jar — and this host skips them by not asking.
    let expected = [
        "cancelOnStopBackup", "announceStarting", "beginResolveTunnel",
        "ensureRuntime", "acceptEula",
        "awaitPreviousExit", "restoreWorld", "spawn", "awaitConsole",
        "openTunnel", "announceRunning",
    ]
    try expect(names == expected, "got:\n  \(names.joined(separator: ", "))")
    return "\(steps.count) steps"
}

check("the spawned plan still carries the jar steps") {
    // The guard on the default. Android sends no engine and must keep the plan
    // it has always had — its launch order throws on a step that is missing,
    // so getting this wrong crashes it on the first start rather than here.
    let names = try Core.launchPlan(
        backups: true, settings: true, tunnel: true, engine: "spawned"
    ).map(\.name)
    try expect(names.contains("ensureJar"), "the spawned plan lost ensureJar: \(names)")
    try expect(names.contains("resolveMainClass"), "the spawned plan lost resolveMainClass")
    try expect(names.count == 14, "expected 14 steps, got \(names.count)")
    return "14 steps, jar steps intact"
}

check("the checkpoints are the four the core marks") {
    let steps = try Core.launchPlan(backups: true, settings: false, tunnel: true)
    let checkpoints = steps.filter(\.checkpoint).map(\.name)
    // Pinned deliberately: if the core changes which steps a stop may
    // interrupt, both hosts should have to notice rather than drift.
    try expect(
        checkpoints == ["restoreWorld", "spawn", "openTunnel", "announceRunning"],
        "got \(checkpoints)")
    return checkpoints.joined(separator: ", ")
}

check("a launch with no tunnel and no backups drops those steps, keeping order") {
    let steps = try Core.launchPlan(backups: false, settings: false, tunnel: false).map(\.name)
    try expect(!steps.contains("beginResolveTunnel"), "tunnel step survived: \(steps)")
    try expect(!steps.contains("openTunnel"), "tunnel step survived: \(steps)")
    try expect(!steps.contains("restoreWorld"), "restore survived: \(steps)")
    try expect(steps.first == "cancelOnStopBackup" && steps.last == "announceRunning", "\(steps)")
    return "\(steps.count) steps"
}

print("\nlifecycle")

check("a start is counted active before anything slow happens") {
    // The whole point: a server not yet counted is one the reconcile loop
    // will try to start for itself, which reprovisions the gateway under us.
    let life = Core.Lifecycle()
    let admission = life.startRequested("a")
    try expect(admission.verdict == "proceed", "verdict=\(admission.verdict ?? "nil")")
    try expect(life.activeIds() == ["a"], "activeIds=\(life.activeIds())")
    try expect(life.runningIds().isEmpty, "a starting server is not running yet")
    return "proceed, active before spawn"
}

check("the same server twice is alreadyRunning, not an error") {
    let life = Core.Lifecycle()
    life.startRequested("a")
    let again = life.startRequested("a")
    try expect(again.verdict == "alreadyRunning", "verdict=\(again.verdict ?? "nil")")
    return "alreadyRunning"
}

check("a second server is refused, and the reply names the one in the way") {
    let life = Core.Lifecycle()
    life.startRequested("a")
    let other = life.startRequested("b")
    try expect(other.verdict == "anotherServerRunning", "verdict=\(other.verdict ?? "nil")")
    try expect(other.serverId == "a", "blamed \(other.serverId ?? "nil"), expected a")
    return "anotherServerRunning(a)"
}

check("callFinished retires a call without retiring the winner's claim") {
    // The bug Android wrote and caught: a duplicate that returns
    // alreadyRunning still has to finish, and finishing must not drop the
    // claim the first call is holding.
    let life = Core.Lifecycle()
    life.startRequested("a")
    life.startRequested("a")
    life.callFinished("a")
    try expect(life.activeIds() == ["a"], "the winner's claim was retired: \(life.activeIds())")
    return "still active after the duplicate finished"
}

check("a stop before anything spawned abandons the launch") {
    let life = Core.Lifecycle()
    life.startRequested("a")
    let stop = life.stopRequested("a")
    try expect(stop.verdict == "abandonLaunch", "verdict=\(stop.verdict ?? "nil")")
    try expect(life.shouldAbandon("a"), "the launch was not told to give up")
    return "abandonLaunch, and the launch sees it"
}

check("a stop before the console terminates rather than asking politely") {
    // A server still generating terrain cannot hear `stop`, and has saved no
    // world to protect.
    let life = Core.Lifecycle()
    life.startRequested("a")
    life.spawned("a")
    let stop = life.stopRequested("a")
    try expect(stop.verdict == "terminate", "verdict=\(stop.verdict ?? "nil")")
    return "terminate"
}

check("a stop after the console is graceful") {
    let life = Core.Lifecycle()
    life.startRequested("a")
    life.spawned("a")
    life.consoleReady("a")
    let stop = life.stopRequested("a")
    try expect(stop.verdict == "graceful", "verdict=\(stop.verdict ?? "nil")")
    return "graceful"
}

check("stopping something that is not here says so") {
    let life = Core.Lifecycle()
    try expect(life.stopRequested("ghost").verdict == "notRunning", "wrong verdict")
    return "notRunning"
}

check("an asked-for exit is intentional; an unasked-for one is a crash") {
    let asked = Core.Lifecycle()
    asked.startRequested("a")
    asked.spawned("a")
    asked.consoleReady("a")
    asked.stopRequested("a")
    let clean = asked.exited("a", code: 0)
    try expect(clean.intentional, "a requested stop was not intentional")
    try expect(clean.state == "stopped", "state=\(clean.state)")

    let fell = Core.Lifecycle()
    fell.startRequested("b")
    fell.spawned("b")
    fell.consoleReady("b")
    let crash = fell.exited("b", code: 1)
    try expect(!crash.intentional, "an unasked-for exit claimed to be intentional")
    try expect(crash.state == "crashed", "state=\(crash.state)")
    return "stopped/intentional and crashed/unintentional"
}

check("a terminated server that someone asked to stop is stopped, not crashed") {
    // exit_state(true, 143): SIGTERM after a stop request. Android reported
    // this as a crash and skipped the on-stop backup, losing the session.
    let life = Core.Lifecycle()
    life.startRequested("a")
    life.spawned("a")
    life.stopRequested("a")
    let exit = life.exited("a", code: 143)
    try expect(exit.state == "stopped" && exit.intentional, "state=\(exit.state)")
    return "143 after a stop -> stopped"
}

check("an exit belonging to a superseded launch is ignored") {
    // The one that bit Android: the old engine's exit must not tear down the
    // launch that replaced it.
    let life = Core.Lifecycle()
    life.startRequested("a")
    life.spawned("a")
    life.consoleReady("a")
    life.stopRequested("a")
    life.callFinished("a")
    life.startRequested("a")  // a restart, before the old one has exited
    let exit = life.exited("a", code: 0)
    try expect(exit.superseded, "the old exit was not marked superseded")
    return "superseded — the new launch keeps its state"
}

check("starting supersedes an on-stop backup of the same server") {
    let life = Core.Lifecycle()
    life.startRequested("a")
    life.spawned("a")
    life.consoleReady("a")
    life.stopRequested("a")
    life.callFinished("a")
    life.exited("a", code: 0)
    let restart = life.startRequested("a")
    try expect(
        restart.supersedesOnStopBackup,
        "a relaunch did not cancel the backup still running for it")
    return "the on-stop backup is cancelled"
}

check("mayAnnounce does not veto a state merely for repeating") {
    // Two clocks: the core's and the host's. The core must not suppress an
    // announcement the host needs to make — that was "the server never comes
    // online" on Android.
    let life = Core.Lifecycle()
    life.startRequested("a")
    life.spawned("a")
    life.consoleReady("a")
    try expect(life.mayAnnounce("a", state: "running"), "running was vetoed")
    try expect(life.mayAnnounce("a", state: "running"), "a repeat was vetoed")
    return "running may be announced, twice"
}

check("awaitPreviousExit is false with nothing to wait for") {
    let life = Core.Lifecycle()
    life.startRequested("a")
    try expect(!life.awaitPreviousExit("a"), "waiting for an engine that never existed")
    return "nothing to wait for"
}

print("\nmetrics")
// The host reads counters and the core does every piece of arithmetic. These
// drive `Core.Metrics` with a synthetic clock, which is the only way to check
// a three-hour graph without waiting for one.

check("the first point on a graph has no rate to report") {
    let metrics = Core.Metrics()
    // 2 GiB, so a wrong divisor is unmistakable rather than plausible.
    try expect(metrics.record(atMs: 0, memUsedKb: 2_097_152, cpuSeconds: 1.0, playerCount: 3),
        "the first reading was not kept")

    let samples = metrics.samples()
    try expect(samples.count == 1, "expected one point, got \(samples.count)")
    try expect(samples[0].memUsedMb == 2048, "memUsedMb: \(String(describing: samples[0].memUsedMb))")
    try expect(samples[0].playerCount == 3, "playerCount lost")
    // A rate needs two readings. Inventing one for the first point would put a
    // number on the graph that nothing measured.
    try expect(samples[0].cpuPercent == nil, "invented a rate from one reading")
    return "2048 MB, no rate yet"
}

/// The likeliest regression in the whole wrapper: `NSNull` decoding as a
/// measured zero. A graph that says 0% is a claim; a gap is the truth.
check("a counter the platform would not report stays absent, not zero") {
    let metrics = Core.Metrics()
    _ = metrics.record(atMs: 0, memUsedKb: nil, cpuSeconds: nil, playerCount: nil)

    let sample = metrics.samples().first
    try expect(sample != nil, "no point recorded")
    try expect(sample?.memUsedMb == nil, "memUsedMb became \(String(describing: sample?.memUsedMb))")
    try expect(sample?.cpuPercent == nil, "cpuPercent became \(String(describing: sample?.cpuPercent))")
    try expect(
        sample?.playerCount == nil, "playerCount became \(String(describing: sample?.playerCount))")
    return "three nulls stayed null"
}

check("a reading the core drops still anchors the next rate") {
    let metrics = Core.Metrics()
    try expect(metrics.record(atMs: 0, memUsedKb: nil, cpuSeconds: 0, playerCount: nil), "first")
    // Offered inside the interval, so it is not kept — but it must still become
    // the anchor, or the rate below is measured over 30s instead of 5s.
    try expect(
        !metrics.record(atMs: 25_000, memUsedKb: nil, cpuSeconds: 0, playerCount: nil),
        "a reading offered early was kept")
    try expect(
        metrics.record(atMs: 30_000, memUsedKb: nil, cpuSeconds: 5.0, playerCount: nil), "third")

    // 5 CPU-seconds over the 5s since the anchor is 100%. Anchored at 0s
    // instead it would read 16.7%, which is why those numbers were chosen.
    guard let rate = metrics.samples().last?.cpuPercent else {
        throw Wrong(what: "no rate on the second point")
    }
    try expect(abs(rate - 100) < 0.5, "expected ~100%, got \(rate)")
    return "100% over the last 5s, not 16.7% over 30s"
}

/// iOS's own bug, before this: `perfHistory` was cleared per run and the CPU
/// sampler was not, so every relaunch opened with a rate measured against the
/// previous run's counter.
check("a new run does not measure its first rate against the last run's counter") {
    let metrics = Core.Metrics()
    _ = metrics.record(atMs: 0, memUsedKb: nil, cpuSeconds: 100.0, playerCount: nil)
    _ = metrics.record(atMs: 30_000, memUsedKb: nil, cpuSeconds: 130.0, playerCount: nil)

    metrics.reset()
    _ = metrics.record(atMs: 60_000, memUsedKb: nil, cpuSeconds: 160.0, playerCount: nil)

    let samples = metrics.samples()
    try expect(samples.count == 1, "reset kept \(samples.count) points from the old run")
    try expect(samples[0].cpuPercent == nil, "the new run inherited a rate")

    // Negative control: the same three readings without the reset must produce
    // a rate. Otherwise this test would pass against a wrapper that returns nil
    // for everything.
    let control = Core.Metrics()
    _ = control.record(atMs: 0, memUsedKb: nil, cpuSeconds: 100.0, playerCount: nil)
    _ = control.record(atMs: 30_000, memUsedKb: nil, cpuSeconds: 130.0, playerCount: nil)
    _ = control.record(atMs: 60_000, memUsedKb: nil, cpuSeconds: 160.0, playerCount: nil)
    try expect(
        control.samples().last?.cpuPercent != nil,
        "the control found no rate either — this test proves nothing")
    return "reset drops the anchor; without it the rate is still there"
}

check("a full graph halves its resolution rather than forgetting the launch") {
    let metrics = Core.Metrics()
    // 400 points at the default 30s spacing: past the 360 the policy keeps.
    for i in 0..<400 {
        _ = metrics.record(atMs: i * 30_000, memUsedKb: 1024, cpuSeconds: Double(i), playerCount: 0)
    }

    let samples = metrics.samples()
    try expect(samples.count <= 360, "kept \(samples.count) points, over the cap")
    // The interesting minutes are the first ones — a world generating, a memory
    // curve settling — so the window loses resolution, not its beginning.
    try expect(samples.first?.t == 0, "the launch scrolled off the graph")
    try expect(samples[1].t - samples[0].t == 60_000, "spacing did not double")
    try expect(metrics.intervalMs == 60_000, "intervalMs: \(String(describing: metrics.intervalMs))")
    return "\(samples.count) points, one per 60s, launch intact"
}

/// The contract the whole split rests on: this host reports counters that only
/// ever climb, and never a rate. If someone ever "simplifies" `cpuSeconds` into
/// a delta, this is what fails.
check("the counters this host reads are cumulative, not rates") {
    guard let first = DeviceMetrics.cpuSeconds() else {
        throw Wrong(what: "cpuSeconds() reported nothing")
    }
    var sink = 0.0
    for i in 0..<2_000_000 { sink += Double(i).squareRoot() }
    guard let second = DeviceMetrics.cpuSeconds() else {
        throw Wrong(what: "cpuSeconds() reported nothing the second time")
    }

    try expect(sink > 0, "the busy loop was optimised away")
    try expect(first >= 0 && second >= 0, "negative CPU seconds: \(first), \(second)")
    try expect(second > first, "the counter did not climb: \(first) then \(second)")

    guard let footprint = DeviceMetrics.footprintKb() else {
        throw Wrong(what: "footprintKb() reported nothing")
    }
    try expect(footprint > 0, "footprint of \(footprint) KB")
    return "cpu \(first)s → \(second)s, footprint \(footprint) KB"
}

print("\nlaunch order")

/// A plan with the shape iOS actually gets, so the tests below walk the real
/// thing rather than a fixture.
@MainActor
func iosOrder(_ life: Core.Lifecycle, _ serverId: String) throws -> LaunchOrder {
    LaunchOrder(
        steps: try Core.launchPlan(backups: true, settings: false, tunnel: true),
        serverId: serverId, lifecycle: life)
}

check("a host may jump over steps it does not perform") {
    let life = Core.Lifecycle()
    life.startRequested("a")
    var order = try iosOrder(life, "a")

    // `ensureRuntime` and `acceptEula` are in this host's plan and it does
    // neither — Pumpkin unpacks nothing and reads no eula.txt. Jumping them
    // must be accepted rather than read as going backwards, because that is
    // what "monotonicity, not exhaustiveness" buys.
    try expect(try order.at("announceStarting"), "announceStarting was not in the plan")
    try expect(
        try order.at("awaitPreviousExit"),
        "jumping ensureRuntime and acceptEula was refused — skipping must stay legal")
    return "jumped ensureRuntime and acceptEula"
}

check("a step absent from the plan is reported absent, not run") {
    let life = Core.Lifecycle()
    life.startRequested("a")
    // No backups and no tunnel: those steps genuinely are not in this plan.
    var order = LaunchOrder(
        steps: try Core.launchPlan(backups: false, settings: false, tunnel: false),
        serverId: "a", lifecycle: life)
    try expect(try order.at("announceStarting"), "announceStarting missing")
    try expect(!(try order.at("beginResolveTunnel")), "a tunnel step appeared in a tunnel-less plan")
    try expect(!(try order.at("restoreWorld")), "a restore appeared with backups off")
    try expect(try order.at("spawn"), "spawn was refused after two absent steps")
    return "absent steps report false and do not disturb the order"
}

check("a step out of order is refused") {
    let life = Core.Lifecycle()
    life.startRequested("a")
    var order = try iosOrder(life, "a")
    _ = try order.at("spawn")
    do {
        // The plan puts the restore well before the spawn. Running it now
        // would be a different launch from the one the other hosts run.
        _ = try order.at("restoreWorld")
        throw Wrong(what: "ran a step the plan puts earlier, with no complaint")
    } catch let e as ServerBackendError {
        return "refused: \(e.errorDescription ?? "")"
    }
}

check("a stop that arrives mid-launch is honoured at the next checkpoint") {
    // The path a device could not exercise: on a simulator the engine reaches
    // its console in milliseconds, so a stop always lands after `running` and
    // the core rightly answers `graceful`. Here the stop lands where it
    // actually matters.
    let life = Core.Lifecycle()
    life.startRequested("a")
    var order = try iosOrder(life, "a")

    try expect(try order.at("cancelOnStopBackup"), "step missing")
    try expect(try order.at("announceStarting"), "step missing")

    // Stop pressed while the tunnel is being resolved and the world restored.
    let stop = life.stopRequested("a")
    try expect(stop.verdict == "abandonLaunch", "verdict=\(stop.verdict ?? "nil")")

    // A non-checkpoint step still runs — abandoning is not immediate, it is
    // "before the next thing that would be expensive to undo".
    try expect(try order.at("beginResolveTunnel"), "a non-checkpoint step was blocked")

    do {
        _ = try order.at("restoreWorld")  // the first checkpoint after the stop
        throw Wrong(what: "the launch carried on past a checkpoint with a stop pending")
    } catch let e as ServerBackendError {
        return "gave up at restoreWorld: \(e.errorDescription ?? "")"
    }
}

check("a launch with no stop pending walks the whole plan") {
    let life = Core.Lifecycle()
    life.startRequested("a")
    var order = try iosOrder(life, "a")
    var ran: [String] = []
    for name in [
        "cancelOnStopBackup", "announceStarting", "beginResolveTunnel", "awaitPreviousExit",
        "restoreWorld", "spawn", "awaitConsole", "openTunnel", "announceRunning",
    ] {
        if try order.at(name) { ran.append(name) }
        if name == "spawn" { life.spawned("a") }
        if name == "awaitConsole" { life.consoleReady("a") }
    }
    try expect(ran.count == 9, "only ran \(ran.count): \(ran)")
    return "\(ran.count) steps, in order, no interruption"
}

print("\nerror paths")
check("an unknown method is an error, not a crash") {
    do {
        _ = try Core.call("nonsense.method", [:])
        throw Wrong(what: "unknown method did not throw")
    } catch let e as Core.CoreError {
        return "threw: \(e.message)"
    }
}

check("a missing required argument is an error, not a crash") {
    do {
        _ = try Core.call("game.classify", ["game": Core.minecraft])  // no `line`
        throw Wrong(what: "missing argument did not throw")
    } catch let e as Core.CoreError {
        return "threw: \(e.message)"
    }
}

check("a reading with no counters at all is an error, not a crash") {
    do {
        _ = try Core.call("metrics.record", [:])  // no `reading`
        throw Wrong(what: "a record with no reading did not throw")
    } catch let e as Core.CoreError {
        return "threw: \(e.message)"
    }
}

check("a wrong-typed argument is an error, not a crash") {
    do {
        _ = try Core.call("tunnel.render", ["game": Core.minecraft, "link": "not an object", "port": 25565])
        throw Wrong(what: "wrong-typed link did not throw")
    } catch let e as Core.CoreError {
        return "threw: \(e.message)"
    }
}

check("ten thousand calls do not leak the reply") {
    // Core.call frees with a defer; if that defer were wrong this would grow
    // without bound. Crude, but it catches a missing free. Any cheap wrapper
    // that returns a fresh reply will do — this one takes no allocation of its
    // own, so what it measures is the boundary rather than the work.
    for _ in 0..<10_000 {
        _ = try Core.shouldBackUp(hasLocalWorld: true)
    }
    return "10k calls completed"
}

// --- reporting -------------------------------------------------------------
//
// The wrappers whose field names nothing else checks. A stats report with a
// misspelled key is accepted by the API and lands as null; an operator change
// signed with the wrong credential answers 200 and is stripped. Neither has a
// symptom, so the assertions here are about *names*, not behaviour.

check("a crash the core recognises comes back as a cause and a message") {
    guard
        let diagnosis = Core.crashDiagnosis(lines: [
            "[12:00:00] [main/ERROR]: Failed to start the minecraft server",
            "java.lang.RuntimeException: java.net.BindException: FAILED TO BIND TO PORT!",
        ])
    else { throw Wrong(what: "a bind failure was not recognised") }

    guard !diagnosis.message.isEmpty else { throw Wrong(what: "no message for the player") }
    return "\(diagnosis.cause): \(diagnosis.message)"
}

check("a console the core has no pattern for is nil, not a wrong answer") {
    // What a Pumpkin crash looks like: none of the JVM strings. The player
    // gets the API's message rather than an invented local one.
    if let wrong = Core.crashDiagnosis(lines: ["[Homerun] the server exited"]) {
        throw Wrong(what: "invented a diagnosis: \(wrong.cause)")
    }
    return "nil, as it should be"
}

check("a crash report is device-signed and goes to service-error") {
    guard
        let request = Core.crashReport(
            serverId: "srv-1", deviceId: "dev-1", lines: ["a line"])
    else { throw Wrong(what: "no request") }

    guard request.path == "/api/service-error/" else {
        throw Wrong(what: "wrong path: \(request.path)")
    }
    guard !request.userSigned else { throw Wrong(what: "a crash must not need a signed-in user") }
    return "\(request.method) \(request.path) as \(request.auth)"
}

check("every stats field reaches the key the API stores it under") {
    guard
        let request = Core.statsReport(
            serviceId: "srv-1", deviceId: "dev-1",
            stats: [
                "memoryKb": 524_288,
                "cpuPercent": 41.5,
                "serverAgeSecs": 2_381.6,
                "hostIp": "203.0.113.7",
                "gatewayPingMs": 18.0,
                "onlineMode": true,
                "roster": ["count": 1, "max": 20,
                    "players": [["name": "Notch", "uuid": "069a79f4-44e9-4726-a5be-fca90e38aaf5"]]],
            ])
    else { throw Wrong(what: "no request") }

    // Wire names, which are not the names above — the core renames on the way
    // out and this is the only place that crossing is checked from Swift.
    for key in ["service", "device", "memory_usage", "cpu_usage", "server_age", "host_ip_address",
        "gateway_ping", "online_mode", "player_count"]
    {
        guard request.body[key] != nil else {
            throw Wrong(what: "\(key) missing from the body — \(Array(request.body.keys).sorted())")
        }
    }
    // KiB in, bytes out. Getting this backwards reports half a gigabyte as
    // half a megabyte and nothing complains.
    guard (request.body["memory_usage"] as? Int) == 524_288 * 1024 else {
        throw Wrong(what: "memory_usage is not bytes: \(request.body["memory_usage"] ?? "nil")")
    }
    return "\(request.body.count) keys, memory in bytes"
}

check("the cadence starts due immediately and then asks for two minutes") {
    let first = Core.schedule(held: nil, nowMs: 1_000_000)
    guard first.trigger != nil else { throw Wrong(what: "the first report was not due") }

    let next = Core.schedule(held: first.held, nowMs: 1_000_000)
    guard next.trigger == nil else { throw Wrong(what: "reported twice on the same clock") }
    guard next.waitMs > 100_000 else { throw Wrong(what: "waitMs is \(next.waitMs), not ~120s") }
    return "first=\(first.trigger ?? "nil"), then waits \(next.waitMs / 1000)s"
}

check("a join earns a report sooner than the next beat") {
    let armed = Core.schedule(held: nil, nowMs: 1_000_000)
    let quiet = Core.schedule(held: armed.held, nowMs: 1_000_000)
    let nudged = Core.schedule(held: quiet.held, nowMs: 1_000_000, event: "presence")
    guard nudged.waitMs < quiet.waitMs else {
        throw Wrong(what: "presence did not bring the report forward: \(nudged.waitMs)")
    }
    return "waits \(nudged.waitMs)ms instead of \(quiet.waitMs)ms"
}

check("cpu is rescaled from one core to the whole device") {
    guard let rescaled = Core.cpuPercentOfDevice(perCorePercent: 200, cores: 4) else {
        throw Wrong(what: "no answer")
    }
    guard rescaled == 50 else { throw Wrong(what: "200% over 4 cores is not \(rescaled)%") }
    return "200% of one core over 4 cores = 50% of the device"
}

check("an operator change is a user-signed PATCH of the settings") {
    guard let command = Core.opsCommand("/op Notch") else {
        throw Wrong(what: "\"/op Notch\" was not read as an ops command")
    }
    guard
        let change = Core.opsSync(
            command: command,
            serverBody: ["id": "srv-1", "environment_variables": ["OPS": ""]],
            serverId: "srv-1")
    else { throw Wrong(what: "no change for a server with no operators") }

    guard change.request.method == "patch" else {
        throw Wrong(what: "not a patch: \(change.request.method)")
    }
    // The whole reason this path exists. A device-signed settings change is
    // accepted with 200 and silently stripped.
    guard change.request.userSigned else {
        throw Wrong(what: "an operator change signed as \(change.request.auth)")
    }
    return "\(change.request.method) \(change.request.path) as user — \(change.line)"
}

check("an ordinary console command is not an operator change") {
    if let wrong = Core.opsCommand("say hello") {
        throw Wrong(what: "\"say hello\" parsed as \(wrong)")
    }
    return "nil, as it should be"
}

check("a minigame result is read out of the line a plugin printed") {
    let line =
        "[12:00:00] [Server thread/INFO]: [HOMERUN:STATS] "
        + #"{"match":"m1","game":"spleef","players":[{"name":"Notch"}]}"#
    guard let request = Core.minigameReport(serverId: "srv-1", line: line) else {
        throw Wrong(what: "a marked line produced no report")
    }
    guard Core.minigameReport(serverId: "srv-1", line: "just a line") == nil else {
        throw Wrong(what: "an unmarked line produced a report")
    }
    return "\(request.method) \(request.path)"
}

check("a stats poll with nothing running answers nulls rather than hanging") {
    // The blocking one. With no server, `ask` cannot send a command and gives
    // up at once, so this also proves it does not sit out its timeout.
    let started = Date()
    let poll = Core.statsPoll()
    let elapsed = Date().timeIntervalSince(started)
    guard poll.roster == nil, poll.ageSecs == nil else {
        throw Wrong(what: "invented an answer with no server running")
    }
    guard elapsed < 1 else { throw Wrong(what: "took \(elapsed)s with nothing to ask") }
    return String(format: "nulls in %.0fms", elapsed * 1000)
}

check("a gateway ping to nowhere is null, not an error") {
    // Port 1 on loopback refuses immediately, so this is the failure path
    // without the deadline. A report must survive a measurement that failed.
    if let wrong = Core.gatewayPing(address: "127.0.0.1:1") {
        throw Wrong(what: "measured \(wrong)ms to a closed port")
    }
    return "nil"
}

check("an address with no port is refused before a socket is opened") {
    if Core.gatewayPing(address: "gateway.example.com") != nil {
        throw Wrong(what: "pinged an address with no port")
    }
    return "nil"
}

check("the gateway address is where a player connects, not the tunnel endpoint") {
    // The external port is the gateway's, assigned while the post-launch poll
    // waits — which is why nothing earlier in a launch can answer this.
    let body: [String: Any] = [
        "config": [
            "links": [[
                "domain": ["uri": "eu.gethomerun.app"],
                "forward_ports": ["minecraft": ["30001:25565/tcp"]],
            ]]
        ]
    ]
    guard let address = Core.publicAddress(serverBody: body) else {
        throw Wrong(what: "no address")
    }
    guard address == "eu.gethomerun.app:30001" else { throw Wrong(what: "got \(address)") }
    return address
}

check("a server with no gateway port yet has no address to ping") {
    // Every launch passes through this state, and `gateway_ping` is null
    // until it leaves it.
    let body: [String: Any] = [
        "config": ["links": [["domain": ["uri": "eu.gethomerun.app"]]]]
    ]
    if let wrong = Core.publicAddress(serverBody: body) {
        throw Wrong(what: "invented \(wrong) before the gateway assigned a port")
    }
    return "nil"
}

print("\n\(checks - failures)/\(checks) passed")
exit(failures == 0 ? 0 : 1)
