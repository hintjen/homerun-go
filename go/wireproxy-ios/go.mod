module wireproxyios

go 1.26.5

require (
	github.com/windtf/wireproxy v1.1.3
	golang.zx2c4.com/wireguard v0.0.0-20260522210424-ecfc5a8d5446
)

require (
	github.com/MakeNowJust/heredoc/v2 v2.0.1 // indirect
	github.com/go-ini/ini v1.67.0 // indirect
	github.com/google/btree v1.1.2 // indirect
	github.com/things-go/go-socks5 v0.0.5 // indirect
	golang.org/x/crypto v0.54.0 // indirect
	golang.org/x/mobile v0.0.0-20260803200217-62cee1672c8e // indirect
	golang.org/x/mod v0.38.0 // indirect
	golang.org/x/net v0.57.0 // indirect
	golang.org/x/sync v0.22.0 // indirect
	golang.org/x/sys v0.47.0 // indirect
	golang.org/x/time v0.7.0 // indirect
	golang.org/x/tools v0.48.0 // indirect
	golang.zx2c4.com/wintun v0.0.0-20230126152724-0fa3db229ce2 // indirect
	gvisor.dev/gvisor v0.0.0-20230927004350-cbd86285d259 // indirect
)

tool golang.org/x/mobile/cmd/gobind

// Our fork. Upstream's RoutineSpawner calls log.Fatal — i.e. os.Exit — on
// every shutdown path, which in-process is the app terminating. See
// wireproxy-fork/PATCHES.md.
replace github.com/windtf/wireproxy => ../../../wireproxy-fork/wireproxy

// The fork's wireproxy uses ClientOnlyBind from the matching wireguard-go
// fork; the two are a pair and must be replaced together.
replace golang.zx2c4.com/wireguard => ../../../wireproxy-fork/wireguard-go
