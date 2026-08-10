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
//         ios/HomerunHost/FFI/Core.swift ios/coretest/main.swift \
//         rust/homerun-pumpkin-ffi/target/release/libhomerun_pumpkin_ffi.a \
//         -o /tmp/coretest && /tmp/coretest
//
// Exits non-zero on the first failure, so it is CI-shaped if anyone wants it
// there. It is not wired in yet: it needs a host Rust build, which the mobile
// CI does not currently do.

import Foundation

var failures = 0
var checks = 0

func check(_ name: String, _ body: () throws -> String) {
    checks += 1
    do {
        let detail = try body()
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
check("homerun_abi_version is 1") {
    let v = homerun_abi_version()
    try expect(v == 1, "expected 1, got \(v)")
    return "v\(v)"
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

print("\nlifecycle")
check("state.exit calls a clean stop stopped") {
    let s = try Core.exitState(intentional: true, code: 0)
    try expect(s == "stopped", "got \(s)")
    return s
}

check("state.exit calls an unexpected exit crashed") {
    let s = try Core.exitState(intentional: false, code: 1)
    try expect(s == "crashed", "got \(s)")
    return s
}

check("state.exit calls exit 0 we did not ask for crashed") {
    let s = try Core.exitState(intentional: false, code: 0)
    return "exit 0, unintentional -> \(s)"
}

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
    // without bound. Crude, but it catches a missing free.
    for _ in 0..<10_000 {
        _ = try Core.exitState(intentional: true, code: 0)
    }
    return "10k calls completed"
}

print("\n\(checks - failures)/\(checks) passed")
exit(failures == 0 ? 0 : 1)
