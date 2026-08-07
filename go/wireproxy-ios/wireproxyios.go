// Package wireproxyios exposes wireproxy to the iOS host as a gomobile-bound
// library, so the tunnel can run inside the app process.
//
// iOS cannot spawn a child process, which is how desktop and Android run
// wireproxy. It also must not need a VPN: WireGuard is terminated in
// wireproxy's own userspace netstack, the interface address is virtual inside
// this process, and nothing is registered with the OS. No NEPacketTunnelProvider,
// no VPN profile, no user prompt — the same property Android's WireProxy.kt
// calls the most important one in the design.
//
// The tunnel is ingress-only. The Homerun gateway is the client:
//
//	player → gateway → WireGuard → <phone wg IP>:25565
//	                 → [TCPServerTunnel] → 127.0.0.1:<minecraft port>
//
// Derived from hintjen/wireproxy-ios, reduced to the tunnel. That project also
// embeds Caddy, Vaultwarden and an SSH server; none of it belongs in a
// Minecraft host, and its dependencies would land in this framework.
package wireproxyios

import (
	"fmt"
	"os"
	"strconv"
	"strings"

	"github.com/windtf/wireproxy"
	"golang.zx2c4.com/wireguard/device"
)

// Tunnel is a running userspace WireGuard interface plus whatever forwarding
// routines its configuration declared.
type Tunnel struct {
	vt *wireproxy.VirtualTun
}

// Start brings up the tunnel described by a wireproxy configuration.
//
// The config is the full INI text — [Interface], [Peer] and the tunnel
// sections — and is rendered by the host so that the desktop, Android and iOS
// all produce byte-for-byte the same thing. It is passed as a string rather
// than a path because it contains a private key; the temp file below exists
// only because wireproxy's parser takes a filename, and is removed
// immediately.
//
// The configuration's own routines are spawned rather than reimplemented here.
// That is deliberate: [TCPServerTunnel] and friends are the contract with the
// gateway, and re-expressing them in Go or Swift is how the two ends drift.
func Start(config string) (*Tunnel, error) {
	file, err := os.CreateTemp("", "homerun-wireproxy-*.conf")
	if err != nil {
		return nil, fmt.Errorf("temp file: %w", err)
	}
	path := file.Name()
	defer os.Remove(path)

	if _, err := file.WriteString(config); err != nil {
		file.Close()
		return nil, fmt.Errorf("write config: %w", err)
	}
	file.Close()

	conf, err := wireproxy.ParseConfig(path)
	if err != nil {
		return nil, fmt.Errorf("parse config: %w", err)
	}

	// Silent: wireguard-go logs to fd 1, and the iOS host redirects that into
	// the player-visible Minecraft console. Handshake health is read from the
	// device below instead of scraped from log lines.
	vt, err := wireproxy.StartWireguard(conf, device.LogLevelSilent)
	if err != nil {
		return nil, fmt.Errorf("start wireguard: %w", err)
	}

	for _, spawner := range conf.Routines {
		go spawner.SpawnRoutine(vt)
	}

	return &Tunnel{vt: vt}, nil
}

// LastHandshakeUnix is when the peer last completed a handshake, in seconds
// since the epoch, or 0 if it never has.
//
// This is the real thing the desktop and Android infer by counting
// "Handshake did not complete after 5 seconds" in wireproxy's stdout. In-process
// there is no child stdout to read, and the device knows the answer exactly.
func (t *Tunnel) LastHandshakeUnix() int64 {
	if t == nil || t.vt == nil || t.vt.Dev == nil {
		return 0
	}
	state, err := t.vt.Dev.IpcGet()
	if err != nil {
		return 0
	}
	for _, line := range strings.Split(state, "\n") {
		value, found := strings.CutPrefix(strings.TrimSpace(line), "last_handshake_time_sec=")
		if !found {
			continue
		}
		seconds, err := strconv.ParseInt(value, 10, 64)
		if err != nil {
			return 0
		}
		return seconds
	}
	return 0
}

// Stop tears the interface down.
//
// Called before stopping the server, not after: a tunnel that outlives its
// server keeps the gateway's peer slot occupied and the next start fails.
func (t *Tunnel) Stop() {
	if t == nil || t.vt == nil || t.vt.Dev == nil {
		return
	}
	t.vt.Dev.Down()
	t.vt.Dev.Close()
}
