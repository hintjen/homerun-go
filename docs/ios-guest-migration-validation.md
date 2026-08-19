# Validating guest-server migration on iOS

A guest builds servers, then signs up or signs in. Their servers should follow
them onto that account. This runbook is how to prove it does on iOS.

Most of the machinery is shared — the API and the UI bundle, both already
exercised on Android. **The only iOS-specific part is device re-registration**
(`DeviceRegistrar.deviceId()`), which was fixed but has never been run on a
real device. That is what this validates.

## Why the device matters

A device row on the backend belongs to **exactly one account**. `deviceId()`
used to return any existing registration regardless of who was signed in, so a
phone that started as a guest stayed registered to the guest it had already
left. Two endpoints then refuse it:

| request | refusal |
| --- | --- |
| `POST /api/push/devices/` | `Not one of your devices.` |
| `POST /api/register/migrate-guest/` | `That device does not belong to this account.` |

The fix records which account a registration belongs to and redoes it when
that changes. Keyed on `matrix_id`, not email — claiming rotates the address
on the same account and device, which must **not** re-register.

## Before you start

1. **Point the build at staging.** Migration endpoints are deployed there, not
   on production. `xcodebuild … HOMERUN_API_URL=https://api.fractalnetworks.co`
   — see [building.md](./building.md). A device that has already run keeps the
   old URL until its data is cleared.
2. **Have a second account that already exists**, so you can test signing in
   as well as registering.
3. **Delete and reinstall the app** between runs. The registration lives in
   `UserDefaults`, so a reinstall is the only clean start.

## The run

1. Launch. A guest is provisioned silently — no sign-in screen.
2. **Create a server.** Wait for it to reach running.
3. Take any upsell (More → backups, the sidebar, the backups panel). They all
   route through `beginSignup` to one screen.
4. Either **register a new account** or **sign in to one that already exists**.
   Both migrate — that is the product decision, and both paths are worth a run.
5. Come back to the app.

Expect: no prompt, no confirmation. The servers move on their own and a toast
says *"Server moved over"*.

## What to check

**On the device** — the marker must follow the account:

```
HostStore.registeredDeviceAccount   ==   HostStore.currentAccount
```

In the Xcode console, on a run that changed account:

```
signed in as a different account; re-registering this device
registered as <new-device-id>
```

If the account did **not** change (an ordinary claim), that line must *not*
appear — re-registering there would orphan the servers already attached.

**In the app**: the migrated server appears on the new account, still running,
at the same address, and starts and stops normally. "Running on another
device" means the host is still a row this account cannot see.

**On the backend**, if you have `ssh stage`, the server should show
`service_owner` = the new account and `current_device` owned by that same
account — not by a guest.

## What failure looks like

| symptom | what it means |
| --- | --- |
| servers migrate but "running on another device" | re-registration did not happen; the API's fallback carried it |
| `push/devices` 400s in the console | the phone is still registered to the previous account |
| no toast, servers left behind | the offer was never captured — check the sign-up screen was reached via `beginSignup` |
| a *second* device row per install after upgrading | the adopt-on-upgrade path is wrong; it should reuse, not re-register |

A useful nuance: **migration can succeed while the device fix is broken.** The
API resolves the host itself when the client names an unusable device — exactly
one candidate, or none. So "the servers moved" is not evidence the iOS fix
works; `current_device`'s owner is.

## Not yet verified

This Swift has **never been compiled** — it was written on a Windows machine
with no Xcode. Build it first and treat a compile error as expected rather than
surprising. The Android equivalent (`DeviceRegistry.ensure`) is the reference
implementation and has been run end to end.
