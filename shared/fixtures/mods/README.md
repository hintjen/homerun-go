# Mod-resolution cases

One file per behaviour. Each is a complete, network-free run of the mod
installer: what the server is configured with, what Modrinth answers, and what
must come out.

## Why these exist

`native-mod-support.md` in the `homerun` repo says it plainly: the desktop
carries **two** hand-maintained copies of this pipeline
(`nativeServerManager.ts` and `mod-installer.ts`), and "a logic fix must be
applied to **both**." Every behaviour recorded here was learned the hard way in
one of them — a mod that resolved to nothing because it only ships betas, a
client-only library that had to be kept because a server mod needed it, a
player's hand-added jar that must never be swept.

`homerun_core::minecraft::mods` is meant to end up being the only
implementation. Until it is, these cases are how the implementations are held
to the same answers: whichever of them a case is run against, it must produce
the `expect` block. A disagreement means one of them is wrong, and the case
says which behaviour is at stake.

## Who reads them today

- **`homerun-core`** — `minecraft::mods::tests::fixtures`, which runs every file in this directory.
- **The desktop** — not yet. The Jest suite at `src/electron/__tests__/mod-installer.test.ts` covers the same ground in its own fixtures; pointing it at these instead is what closes the loop, and the format below is fixed so that it can be.

## Format

```jsonc
{
  "name":  "kebab-case, unique",
  "why":   "the behaviour at stake, in a sentence — this is the point of the file",
  "inputs": {                       // mods::Inputs
    "loader": "fabric",
    "gameVersion": "1.21.4",
    "projects": "geyser",           // MODRINTH_PROJECTS, verbatim
    "excluded": "",                 // EXCLUDED_IDS, verbatim
    "existing": {},                 // the `mods` map already in .homerun-loader.json
    "modpackFiles": [],
    "modpackProjects": [],
    "present": []                   // filenames already in mods/ or plugins/
  },
  "modrinth": {                     // matched as a SUBSTRING of the request URL,
    "/project/geyser/version": []   // first match wins; anything unmatched fails
  },
  "unfetchable": [],                // filenames whose download fails
  "expect": {
    "installed":  [],               // slugs, in order
    "downloaded": [],               // filenames actually fetched, in order
    "failed":     [],               // { slug, reason }
    "remove":     [],               // filenames swept
    "records":    {}                // the `mods` map written back
  }
}
```

`modrinth` is keyed by URL substring rather than by exact URL on purpose: the
query string is percent-encoded and an implementation should not have to
reproduce it byte-for-byte to answer a case. Where the encoding *is* the point,
there is a separate unit test for it.

A key that matches nothing is not silently ignored — an unmatched request is
answered as a failure, so a case that stops exercising what it claims to fails
rather than passing quietly.
