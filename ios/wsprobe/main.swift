// Does this device actually serve a websocket? A simulator harness for the
// half of `plans/device-websocket.md` that needs no account.
//
// `ios/coretest` proves the *decisions* — it links the host build, where
// `device-ws` is off, so it can say nothing about the socket. This runs the
// real listener out of the simulator staticlib and talks to it: bind, upgrade,
// refuse an unauthenticated peer, stop. That is D2's auth contract, and it is
// the part that needs neither a Homerun account nor the gateway.
//
// # Running it
//
//     npm run rust:ios-sim
//     xcrun simctl boot "iPhone 17"          # any booted device will do
//     swiftc -target arm64-apple-ios16.0-simulator \
//         -sdk "$(xcrun --sdk iphonesimulator --show-sdk-path)" \
//         -import-objc-header ios/HomerunHost/FFI/HomerunFFI.h \
//         ios/wsprobe/main.swift ios/HomerunHost/lib/sim/libhomerun_pumpkin_ffi.a \
//         -o /tmp/wsprobe
//     xcrun simctl spawn booted /tmp/wsprobe
//
// Exits 0 only when the refusal arrives as close 4001. Expected output:
//
//     abi 8
//     start -> {"ok":true,"port":56698,"tlsPort":56699}
//     tcp connected to 127.0.0.1:56698
//     handshake -> HTTP/1.1 101 Switching Protocols
//     frame opcode 1: {"message":"Not authenticated","type":"error"}
//     unauthenticated frame -> close 4001 not authenticated
//     stop -> {"ok":true}
//
// The WebSocket handshake is written out by hand over a BSD socket on purpose.
// `URLSessionWebSocketTask` was tried first and reported nothing at all — no
// open, no close, no error — so the one thing under test was the one thing it
// could not tell us about.
//
// **What this does not prove**: the link, the certificate, or anything that
// needs the API. Those need an account and the gateway; see the plan.
import Foundation

func text(_ raw: UnsafeMutablePointer<CChar>?) -> String {
    guard let raw else { return "<null>" }
    defer { homerun_free_string(raw) }
    return String(cString: raw)
}

print("abi \(homerun_abi_version())")

let config = """
{"port":0,"apiUrl":"https://api.gethomerun.app",\
"jwksUrl":"https://auth.gethomerun.app/realms/FractalKeycloak/protocol/openid-connect/certs",\
"deviceId":"probe","expectProxyProtocol":false}
"""
let started = text(config.withCString { homerun_device_ws_start($0) })
print("start -> \(started)")

guard let data = started.data(using: .utf8),
    let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
    object["ok"] as? Bool == true, let port = object["port"] as? Int
else { print("FAIL: no socket"); exit(1) }

let fd = socket(AF_INET, SOCK_STREAM, 0)
var addr = sockaddr_in()
addr.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
addr.sin_family = sa_family_t(AF_INET)
addr.sin_port = UInt16(port).bigEndian
addr.sin_addr.s_addr = inet_addr("127.0.0.1")
let connected = withUnsafePointer(to: &addr) {
    $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
        connect(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
    }
}
guard connected == 0 else { print("FAIL: connect: \(String(cString: strerror(errno)))"); exit(1) }
print("tcp connected to 127.0.0.1:\(port)")

// A minimal RFC 6455 client handshake. The key is fixed: the server's accept
// value is not checked here, only that it upgrades.
let request = """
GET / HTTP/1.1\r
Host: 127.0.0.1:\(port)\r
Upgrade: websocket\r
Connection: Upgrade\r
Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r
Sec-WebSocket-Version: 13\r
\r

"""
_ = request.withCString { send(fd, $0, strlen($0), 0) }

var timeout = timeval(tv_sec: 12, tv_usec: 0)
setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))

var buffer = [UInt8](repeating: 0, count: 4096)
let handshake = recv(fd, &buffer, buffer.count, 0)
guard handshake > 0 else { print("FAIL: no handshake reply"); exit(1) }
let head = String(bytes: buffer[0..<handshake], encoding: .utf8) ?? ""
print("handshake -> \(head.split(separator: "\r\n").first.map(String.init) ?? "?")")

// Now say something without authenticating first. The contract is close 4001,
// and the close frame carries the code in its first two payload bytes.
// Client frames must be masked: 0x81 (fin+text), 0x80|len, 4 mask bytes, payload.
let payload = Array("{\"type\":\"subscribe-logs\",\"serverId\":\"whatever\"}".utf8)
var frame: [UInt8] = [0x81, UInt8(0x80 | payload.count)]
let mask: [UInt8] = [0x37, 0xfa, 0x21, 0x3d]
frame += mask
frame += payload.enumerated().map { $0.element ^ mask[$0.offset % 4] }
_ = frame.withUnsafeBufferPointer { send(fd, $0.baseAddress, frame.count, 0) }

var closeCode: Int?
var reason = ""
while true {
    let n = recv(fd, &buffer, buffer.count, 0)
    if n <= 0 { break }
    let opcode = buffer[0] & 0x0F
    if opcode == 0x8, n >= 4 {
        closeCode = Int(buffer[2]) << 8 | Int(buffer[3])
        reason = String(bytes: buffer[4..<Int(n)], encoding: .utf8) ?? ""
        break
    }
    print("frame opcode \(opcode): \(String(bytes: buffer[2..<Int(n)], encoding: .utf8) ?? "")")
}
close(fd)

print("unauthenticated frame -> close \(closeCode.map(String.init) ?? "none") \(reason)")
print("stop -> \(text(homerun_device_ws_stop()))")
exit(closeCode == 4001 ? 0 : 2)
