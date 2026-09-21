# Windows network enforcement validation — 2026-09-21

Scope: engine issues #31 and #37. No new Facepunch Rust launch was performed with
this change. Existing successful Rust evidence remains tied to runner cb7ed88118d8.

Observed before listener tests were stopped:

- Runner protocol/unit suite: 10 passed.
- Lifecycle suite: 34 of 35 entries passed, including all seven new network tests:
  exposure without a marker; widening after ready; widening during stop; owned
  descendant exposure; late undeclared loopback versus wildcard inventory; unread
  runner output; descendants gone before a clean stop event.
- The remaining existing test assumed its undeclared grandchild had already bound
  when the parent announced ready. Faster native inspection exposed that setup race.
  The fixture now declares that descendant port so readiness waits for it. This
  correction was compiled but its execution has not been repeated.
- Earlier native Windows snapshot/error tests: 2 passed (TCP IPv4 and UDP IPv6,
  owner filtering, API errors and bounded resize failure). Additional TCP IPv6 and
  UDP IPv4 assertions were added afterwards; those final assertions are compile-only.
- Core binding policy test passed, including IPv4, IPv6 and mapped loopback,
  wildcard/LAN refusal and TCP/UDP separation.
- Deliberately disabling the watcher made the pre-marker test fail: ready_timeout
  replaced the required port_exposed. Disabling only undeclared-wide refusal made
  verify return ok despite the wildcard endpoint appearing in its inventory.
  Both mutations were restored from saved bytes before the subsequent test run.

A read-only review found and corrected watchdog blocking on event output and the
root-exit/descendant-cleanup gap. Subsequent review found no further production
blocker and tightened the two closure assertions to immediate socket checks.

The first sandboxed full-suite run also failed the pre-existing doctor RAM assertion
because CIM access was denied. A standard-user retry outside the sandbox passed;
the token was not elevated. Supervisor unit compilation exposed two stale
RunRequest initializers missing local_network; both now set false explicitly.

Justin reported five or six Windows Firewall prompts for fake.exe. The tests use
fresh executable copies in separate temporary directories. All listener tests were
stopped and a process inspection found none remaining. Justin explicitly chose
“Use existing results; finish review and push” instead of another run. The agent
accepted no prompt and changed no Firewall rule. Fixture prompts do not establish
anything about RustDedicated.exe prompting in a new run.

Final checks are compilation only. Outstanding before treating this as fully verified:
rerun the complete lifecycle/supervisor suites with an agreed fixture-prompt strategy,
and run Rust verify on the new build. Polling detects exposure after bind, not before;
short-lived endpoints may be missed. UDP inventory includes outbound bound endpoints.
No published runner, Rust verdict upgrade, integration, or issue closure is implied.
