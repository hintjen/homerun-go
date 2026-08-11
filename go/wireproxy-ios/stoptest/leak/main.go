// Command leak identifies which goroutines survive Tunnel.Stop().
//
// Diagnostic only — run it by hand when the cycle count in stoptest drifts.
package main

import (
	"fmt"
	"os"
	"runtime"
	"runtime/pprof"
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

func main() {
	before := runtime.NumGoroutine()

	for i := 0; i < 3; i++ {
		t, err := wireproxyios.Start(config)
		if err != nil {
			fmt.Println("start failed:", err)
			os.Exit(1)
		}
		time.Sleep(1500 * time.Millisecond)
		t.Stop()
		time.Sleep(1500 * time.Millisecond)
	}

	runtime.GC()
	time.Sleep(500 * time.Millisecond)
	fmt.Printf("goroutines before=%d after 3 cycles=%d\n\n", before, runtime.NumGoroutine())
	_ = pprof.Lookup("goroutine").WriteTo(os.Stdout, 1)
}
