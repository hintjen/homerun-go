// Command stoptest proves that stopping a tunnel does not kill the process,
// and that a tunnel can be started and stopped repeatedly without leaking
// goroutines or failing to rebind its listener.
//
// The bug this guards: Tunnel.Stop() closed the WireGuard device out from under
// the routines wireproxy had spawned. The [TCPServerTunnel] accept loop woke
// with "endpoint is in invalid state" — gVisor netstack's wording, not
// net.ErrClosed — and wireproxy answered that with log.Fatal, i.e. os.Exit(1).
// In-process on a phone that is the whole app disappearing.
//
// No peer and no network are needed: the endpoint is TEST-NET-1 (192.0.2.1),
// which never answers, so the handshake never completes. The accept loop is
// still bound and still spawned, which is all it takes to reproduce.
//
// If the process dies before printing SURVIVED, the bug is back. If the
// goroutine count climbs cycle over cycle, Stop is leaking. If a later cycle
// fails to Start, the listener is not being released.
package main

import (
	"fmt"
	"os"
	"runtime"
	"time"

	"wireproxyios"
)

const config = `[Interface]
PrivateKey = UDy1t3G2t0deMNd/xrRb6+/Qmy4l/md/FmFhCMlSXn0=
Address = 10.0.0.2/32

[Peer]
PublicKey = Z1sVr5AX4jiXKrrwnAf6GpaCF3H2Jx8V6/Cus6OPWUk=
Endpoint = 192.0.2.1:51820
AllowedIPs = 0.0.0.0/0
PersistentKeepalive = 25

[TCPServerTunnel]
ListenPort = 25565
Target = 10.0.0.1:25565
`

const cycles = 5

func main() {
	var baseline int

	for i := 1; i <= cycles; i++ {
		t, err := wireproxyios.Start(config)
		if err != nil {
			// A failure on cycle 2+ means the previous Stop did not release
			// the port — exactly the rebind bug worth watching for.
			fmt.Printf("FAIL: Start on cycle %d: %v\n", i, err)
			os.Exit(1)
		}

		// Let the accept loop actually reach its blocking Accept before we pull
		// the device away — the race only reproduces once it is parked there.
		time.Sleep(1500 * time.Millisecond)
		t.Stop()
		time.Sleep(1500 * time.Millisecond)

		runtime.GC()
		n := runtime.NumGoroutine()
		if i == 1 {
			baseline = n
		}
		fmt.Printf("cycle %d: survived stop, goroutines=%d (drift %+d)\n", i, n, n-baseline)
	}

	final := runtime.NumGoroutine()
	fmt.Printf("SURVIVED: %d start/stop cycles, still alive, goroutines=%d\n", cycles, final)
	if final-baseline > cycles {
		fmt.Printf("WARN: goroutine count grew by %d across %d cycles — possible leak\n",
			final-baseline, cycles)
		os.Exit(2)
	}
}
