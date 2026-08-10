//go:build tools

// Package tools pins restic so the Android build is reproducible.
//
// restic publishes no android/arm64 release — Android needs GOOS=android for
// bionic, not GOOS=linux — so the binary is built from source here. The import
// below is what keeps the version in go.mod; nothing links this file.
//
// **The version must match what the desktop ships.** `download-assets.js` in
// the `homerun` repo pins the same one, and both write to the same
// repositories. Bumping one without the other is how two clients end up
// disagreeing about a format.
package tools

import _ "github.com/restic/restic/cmd/restic"
