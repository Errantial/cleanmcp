# Competitive release-readiness plan (multi-draft)

> Status (v1.0.0, 2026-11-05): Phase 0 done in-repo (`.gitignore` support, error surfacing,
> README matrix, community files, cargo-deny, `scripts/bench/`); Phase 1 engineering done
> (macOS targets + CI, attestations + SBOM, nightly builds, `server.json`); Phase 2 partially
> (integration tests, snapshot compat kept). Still open — all need accounts/approval outside
> this repo: crates.io publish, winget PR, AUR upload, Homebrew tap, MCP registry publish,
> cargo-binstall metadata, .deb/.rpm, self-update.

Goal: make cleanmcp **competitively release-ready** — i.e. not just "a working tool", but the
obvious choice versus `dua`, `dust`, `gdu`, `ncdu` and TreeSize-class tools, on every platform
users actually run, installable through every channel they actually use, and discoverable by AI
agents through the MCP ecosystem.

This document keeps three drafts. Draft 1 is the raw brainstorm; Draft 2 reorganizes it into
workstreams and fills the gaps the first pass missed; Draft 3 is the recommended, phased plan
with acceptance criteria. Revision notes between drafts say what changed and why.

Current state (v0.6.0, released 2026-11-04): CLI (`tree`, `files`, `ext`, `age`, `dupes`,
`suggest`, `drives`, `rules`, `export`, `snapshot`, `diff`), interactive TUI (`ui`), MCP server
(8 read-only tools + opt-in `move_to_trash`, stdio and HTTP), ~80 cleanup rules, config file,
109 tests, CI (fmt/clippy/test on Ubuntu+Windows, MSRV 1.88), release workflow producing
Windows-msvc + Linux-musl (x86_64/aarch64) archives with SHA256SUMS, install scripts, Scoop
bucket manifest and AUR PKGBUILD kept in-repo. Benchmarks (`docs/benchmarks.md`): fastest of
dua/dust/gdu on a 957k-entry tree, parity on a 6.1M-entry profile, lowest CPU of the fast
tools — all measured on Windows only.

---

## Draft 1 — raw brainstorm

Unordered first pass over "what would make this competitive":

- Ship macOS builds (every competitor does; we currently ship Windows + Linux only).
- Publish to crates.io so `cargo install cleanmcp` works.
- Get into winget, Homebrew, AUR for real (not just manifests in this repo).
- List the MCP server in MCP registries (the official MCP registry, Glama, Smithery) so agents find it.
- Sign releases / add attestations and an SBOM; users and CI can verify provenance.
- Reduce memory further — we are at ~87 B/entry vs dua's ~52.
- Benchmark on Linux and macOS too; all our numbers are Windows-only.
- Add a perf-regression gate in CI so we never silently get slower.
- Respect `.gitignore` / `.ignore` files during scans (dust/gdu-class behavior).
- More docs: comparison table vs competitors, screenshots/GIF of the TUI, FAQ.
- CONTRIBUTING.md, issue templates, SECURITY.md, code of conduct.
- cargo-deny for license/advisory checks in CI.
- More integration tests for the CLI commands, not just unit tests.
- Nightly/dev builds from main so people can try unreleased features.
- Changelog automation (git-cliff) and a documented versioning policy.
- Reproducible builds.
- A `cleanmcp self-update` command.
- Windows ARM64 build.
- .deb/.rpm packages or cargo-binstall metadata so `cargo binstall cleanmcp` works.
- GitHub Sponsors / funding.
- Discord or Matrix community.
- Localization.
- NTFS MFT fast-scan mode on Windows (admin) for whole-drive scans.
- `du`-compatible output mode for script compatibility.
- Export to ncdu/gdu formats for interop.

---

## Draft 2 — revised: workstreams with priorities

Revision notes: Draft 1 mixed "must have for launch" with "nice someday" and missed several
things entirely (see gaps below). Draft 2 groups everything into eight workstreams, assigns
P0 (blocks "release ready") / P1 (strongly competitive) / P2 (differentiator, schedule later),
and adds the gaps found while reviewing Draft 1.

### Gaps found when reviewing Draft 1

- No mention of **Linux/macOS CI testing** — we only test Ubuntu + Windows; a macOS build would ship untested.
- No mention of **MCP server packaging metadata** (`server.json` / `mcp-publisher`) — registry listing needs it, not just a URL.
- No mention of **error surfacing**: scans count `errors` (permission-denied etc.) — the CLI/TUI should show "N entries unreadable" so results are trustworthy on real systems.
- No mention of **README comparison table** — the single most-visited page needs a "cleanmcp vs dua/dust/gdu/ncdu/TreeSize" matrix, not just install instructions.
- No mention of **update notifier** in the CLI (know when a new release exists).
- Draft 1's "localization" and "community chat" are P2 at best; demoted.

### W1. Performance & memory (our core claim)

| # | Item | Priority | Notes |
|---|---|---|---|
| W1.1 | Cross-platform benchmark suite: same fixture tree + interleaved runs vs dua/dust/gdu on Windows, Linux, macOS; script committed in-repo (`scripts/bench/`) so anyone can reproduce; results appended to `docs/benchmarks.md` | P0 | All current numbers are Windows-only; competitors are strong on Linux |
| W1.2 | Memory reduction toward dua's ~52 B/entry (arena for names, shrink `Node` further, or store mtime more compactly) | P1 | We are at ~87; on a 10M-entry tree that is ~350 MB difference |
| W1.3 | Perf-regression CI job (nightly or on PRs touching scan/query): run the bench script on a fixed runner, fail if >10% slower than a stored baseline | P1 | Protects the claim that makes us competitive |
| W1.4 | Optional NTFS MFT whole-drive scan on Windows (admin, opt-in flag) | P2 | Big win on huge NTFS volumes; needs admin and careful testing |
| W1.5 | Investigate `jwalk`/`io_uring` on Linux if W1.1 shows us behind on Linux | P2 | Only if benchmarks say so |

### W2. Platforms

| # | Item | Priority | Notes |
|---|---|---|---|
| W2.1 | macOS builds: add `x86_64-apple-darwin` + `aarch64-apple-darwin` to the release matrix, archives, install.sh, CI test job | P0 | Every competitor ships macOS; we ship none |
| W2.2 | Windows ARM64 (`aarch64-pc-windows-msvc`) build | P2 | Growing segment; cheap once the matrix is parameterized |
| W2.3 | Verify Linux glibc build in addition to musl (or document musl-only) | P1 | Some users want glibc for plugin/driver reasons |

### W3. Distribution channels

| # | Item | Priority | Notes |
|---|---|---|---|
| W3.1 | crates.io publish (`cargo install cleanmcp`) — needs `cargo publish` dry-run in CI, license files, README, docs.rs | P0 | The default Rust install path |
| W3.2 | winget PR to `microsoft/winget-pkgs` | P0 | Default Windows package manager for many |
| W3.3 | Homebrew tap (`homebrew-cleanmcp`) with formula for macOS+Linux | P1 | Default on macOS |
| W3.4 | AUR upload (`cleanmcp-bin`) — PKGBUILD already in repo | P1 | Arch users |
| W3.5 | Scoop: PR the manifest to the official bucket or keep the custom bucket documented prominently | P1 | Decide: official bucket = more users, more review friction |
| W3.6 | cargo-binstall support: publish binstall metadata (or rely on crates.io + prebuilt release lookup) so `cargo binstall cleanmcp` gets our GitHub binaries | P1 | Fast installs without compiling |
| W3.7 | .deb/.rpm via a packaging workflow (e.g. cargo-deb / nfpm) | P2 | Nice for servers; binstall covers most |
| W3.8 | Chocolatey package | P2 | Windows enterprise |

### W4. MCP ecosystem (our unique lever)

| # | Item | Priority | Notes |
|---|---|---|---|
| W4.1 | Add MCP server manifest (`server.json`) and publish to the official MCP registry via `mcp-publisher` in the release workflow | P0 | This is how agents discover tools; competitors have no MCP story |
| W4.2 | Listings on Glama and Smithery | P1 | Secondary discovery |
| W4.3 | `docs/mcp-setup.md` one-liners for every major client (Claude Code/Desktop, VS Code, Cursor, Windsurf, Zed, Continue) | P1 | Setup friction kills adoption |
| W4.4 | HTTP transport hardening docs: when to expose beyond loopback, reverse-proxy guidance | P2 | We refuse `--allow-delete` off-loopback; document the safe patterns |

### W5. Release trust & engineering

| # | Item | Priority | Notes |
|---|---|---|---|
| W5.1 | Provenance: GitHub artifact attestations (`actions/attest-build-provenance`) + optional cosign signing of SHA256SUMS | P0 | Cheap, big trust win |
| W5.2 | SBOM (cargo-sbom / syft) attached to each release | P1 | Enterprise users ask for this |
| W5.3 | Reproducible builds: pin toolchain in release workflow, verify two builds produce identical hashes (at least for one target) | P2 | Hard with cross-compilation; start with one target |
| W5.4 | Nightly builds from main (workflow_dispatch already supports `dev-<sha>`; add a scheduled nightly with a `nightly` release or pre-release) | P1 | Lets early adopters test; also feeds W1.3 |
| W5.5 | Changelog automation (git-cliff) + `CHANGELOG.md` policy + semantic versioning doc | P1 | We hand-write changelogs well; automate the mechanical part |
| W5.6 | `cleanmcp self-update` (download latest release, verify SHA256SUMS + attestation, replace binary) | P2 | install scripts already fetch; self-update completes the loop |
| W5.7 | Update notifier in CLI/TUI (check latest GitHub release once a day, stderr note) | P2 | Off by default? No — on, but cache and never block |

### W6. Product features that close the gap

| # | Item | Priority | Notes |
|---|---|---|---|
| W6.1 | Respect `.gitignore` / `.ignore` / global gitignore during scans, with `--no-ignore` to disable and `--exclude` as today | P0 | Table stakes vs dust/gdu; dev trees are the common case |
| W6.2 | Surface scan errors in output: "N entries unreadable (permission denied)" in CLI summary, TUI badge, MCP scan result | P0 | Trust: silent skips make totals wrong-looking |
| W6.3 | `du`-compatible mode (`cleanmcp du [PATH]` printing `size\tpath` like `du -ab`) | P1 | Script interop, zero cost |
| W6.4 | Export/import interop: read gdu/ncdu JSON exports, write them | P2 | Migration path for gdu/ncdu users |
| W6.5 | File-count aggregation view (dua-style "aggregate by count") in TUI and `tree --by count` | P2 | We have counts in the TUI already; expose in CLI |

### W7. Docs & UX

| # | Item | Priority | Notes |
|---|---|---|---|
| W7.1 | README comparison matrix: cleanmcp vs dua / dust / gdu / ncdu / TreeSize (features, platforms, MCP, safety) + link to benchmarks | P0 | First impression decides |
| W7.2 | TUI screenshot/GIF in README and `assets/` | P1 | Shows, doesn't tell |
| W7.3 | FAQ: "why not just dua?", "is it safe for agents?", "how does deletion work?", "Windows vs Linux differences" | P1 | Pre-empts the two biggest objections |
| W7.4 | Man page already generated; verify it covers every flag; `--help` epilog with examples | P1 | Polish |
| W7.5 | Benchmarks page linked from README with the reproducible script | P1 | Backs the performance claim |

### W8. Community & process

| # | Item | Priority | Notes |
|---|---|---|---|
| W8.1 | `CONTRIBUTING.md` (dev setup, test commands, PR process), issue templates (bug/feature/rule request), PR template | P0 | Unblocks outside contributions |
| W8.2 | `SECURITY.md` (how to report, what counts as a vuln — e.g. path-escape in MCP `--root`, trash escaping symlinks) | P0 | We expose a deletion path to agents; a security policy is mandatory |
| W8.3 | CODE_OF_CONDUCT.md | P1 | Standard |
| W8.4 | cargo-deny (licenses + advisories) in CI; `deny.toml` | P1 | Supply-chain hygiene |
| W8.5 | More integration tests: drive every CLI subcommand against a fixture tree (assert JSON output), snapshot round-trip, diff, config loading | P1 | We have 109 unit tests but only one integration file |
| W8.6 | Fuzz/property tests for snapshot format and glob matching (cargo-fuzz or proptest) | P2 | Snapshot format is a compatibility surface |
| W8.7 | GitHub Sponsors / FUNDING.yml | P2 | Sustainability |
| W8.8 | Discussions enabled; label taxonomy | P2 | Community |

---

## Draft 3 — recommended plan (phased)

Revision notes: Draft 2 was a flat priority list; it didn't say *in what order* to do things or
what "done" means. Draft 3 sequences the P0/P1 items into three phases ending at a "1.0
release-ready" state, gives each phase acceptance criteria, and explicitly defers P2. The
ordering principle: **trust and reach before features** — a tool agents and package managers
can install and verify beats a slightly faster scanner nobody can find.

### Phase 0 — quick wins (target: v0.7.0, ~2–3 weeks)

Everything here is small, independent, and immediately visible to users.

1. W6.1 `.gitignore`/`.ignore` support (+ `--no-ignore`), with tests on a fixture tree.
2. W6.2 scan-error surfacing in CLI summary, TUI, and MCP `scan_directory` result.
3. W7.1 README comparison matrix + link to `docs/benchmarks.md`.
4. W7.2 TUI GIF/screenshot in `assets/` + README.
5. W8.1 CONTRIBUTING.md, issue/PR templates.
6. W8.2 SECURITY.md.
7. W8.4 cargo-deny in CI.
8. W1.1 benchmark script committed under `scripts/bench/`; first Linux run recorded in `docs/benchmarks.md` (Windows numbers already exist).

Acceptance criteria: v0.7.0 released; `docs/benchmarks.md` has at least Windows + Linux
numbers; CI green including cargo-deny; README shows the comparison table.

### Phase 1 — reach (target: v0.8.0–v0.9.0, ~1–2 months)

Distribution and platform breadth — the "can I install it?" phase.

1. W2.1 macOS: release-matrix targets `aarch64-apple-darwin` + `x86_64-apple-darwin`,
   install.sh branch, CI test job on `macos-latest`; record macOS benchmarks (W1.1).
2. W3.1 crates.io publish (CI dry-run gate, docs.rs metadata).
3. W3.2 winget PR; W3.4 AUR upload; W3.3 Homebrew tap; W3.5 Scoop decision (official bucket PR).
4. W3.6 cargo-binstall metadata so prebuilt binaries are used.
5. W4.1 MCP registry: `server.json` + `mcp-publisher` in the release workflow; W4.2 Glama/Smithery; W4.3 setup docs for all major clients.
6. W5.1 attestations on release artifacts; W5.4 nightly builds; W5.5 git-cliff + versioning doc.
7. W1.2 memory reduction pass (target ≤ 70 B/entry) with before/after in `docs/benchmarks.md`.
8. W1.3 perf-regression CI job (nightly, baseline stored in repo).
9. W6.3 `du`-compatible mode; W7.3 FAQ; W7.4 help/man polish.
10. W8.5 CLI integration tests over a fixture tree for every subcommand.

Acceptance criteria: installable via `cargo install cleanmcp`, `winget install`, `brew install`,
`scoop install`, `pacman -U`/AUR, `cargo binstall cleanmcp`, and the install scripts, on
Windows x64, Linux x64/arm64, macOS x64/arm64; MCP server listed in the official registry;
release artifacts carry attestations; nightly builds published; perf gate active.

### Phase 2 — "1.0 release ready" (target: v1.0.0)

The point where we declare stability and commit to compatibility.

1. W5.2 SBOM per release; W5.3 reproducible-build verification for at least the Linux x64 target.
2. W5.6 `self-update`; W5.7 update notifier.
3. W6.4 gdu/ncdu export-import interop; W6.5 count aggregation in CLI.
4. W8.6 proptest/fuzz on snapshot format + globs; snapshot format v1 frozen (documented, with a compatibility promise).
5. W2.2 Windows ARM64; W2.3 glibc Linux build decision; W3.7 .deb/.rpm; W3.8 Chocolatey.
6. W1.4/W1.5 only if the cross-platform benchmarks (W1.1/W1.3) show a real gap.
7. W7 docs complete: FAQ, security model doc, MCP HTTP deployment guide (W4.4).
8. W8.7 funding, W8.8 discussions + label taxonomy.
9. Public 1.0 announcement: README badge, release notes, comparison table, benchmark page.

Acceptance criteria (the "release ready" definition):
- **Performance:** on Windows, Linux and macOS, cleanmcp is within 10% of the fastest of
  dua/gdu on a ≥1M-entry tree (wall, warm, min of 3, interleaved), with the lowest CPU, and
  memory ≤ 70 B/entry. Numbers reproducible via `scripts/bench/` and published.
- **Reach:** all Phase-1 install channels live; MCP server in the official registry and ≥2
  agent directories; setup docs for ≥6 clients.
- **Trust:** every release has SHA256SUMS + attestations + SBOM; install scripts verify
  checksums; SECURITY.md with a tested report path; cargo-deny clean; CI green on all
  platforms incl. MSRV; perf gate active.
- **Stability:** snapshot format frozen with a compatibility promise; ≥90% of CLI surface
  covered by integration tests; versioning policy documented.
- **Docs:** README comparison matrix, TUI demo, FAQ, benchmarks — a new user can decide in
  2 minutes why cleanmcp over dua/dust/gdu/ncdu/TreeSize.

### Explicit non-goals (for now)

- Localization/i18n, community chat server, permanent-delete features, GUI app, MFT scanning
  unless benchmarks demand it, .deb/.rpm before binstall proves insufficient.
- We will **not** add agent-autonomous deletion by default; the read-only-by-default MCP model
  is the differentiator and the safety story.

### Risks

| Risk | Mitigation |
|---|---|
| macOS/Linux benchmarks reveal we are slower there (all our numbers are Windows) | W1.1 runs first in Phase 0; fix before claiming parity; jwalk/io_uring investigation is pre-approved (W1.5) |
| Registry/ package-manager review processes reject us (winget, official Scoop bucket) | Keep custom channels working (install scripts, custom bucket, AUR, Homebrew tap); treat official channels as best-effort |
| Memory reduction destabilizes the tree code | It is covered by 109 unit tests + new integration tests; ship behind a flag first if needed |
| MCP registry listing requires manifest maintenance per release | Automate in the release workflow (W4.1), not by hand |
| Scope creep in Phase 2 | P2 items only enter if a Phase-2 acceptance criterion is unmet |

### How to verify progress

- `scripts/bench/` reproduces every number in `docs/benchmarks.md`.
- CI badge matrix in README shows fmt/clippy/test/msrv/deny/perf-gate across OSes.
- The Phase-2 acceptance list above is the checklist for the v1.0.0 release notes.

---

## Appendix A — competitor matrix to verify before publishing

Verify against current releases before putting this in the README (flags change):

| Feature | cleanmcp | dua | dust | gdu | ncdu | TreeSize |
|---|---|---|---|---|---|---|
| Platforms | Win, Linux (macOS in Phase 1) | Win, Linux, macOS | Win, Linux, macOS | Win, Linux, macOS | Linux, macOS, Win | Windows |
| Scan speed (1M+ entries) | fastest/parity (see benchmarks) | fast | slow on Windows | fast | n/a | GUI |
| Interactive TUI | `ui` | yes | no | yes | yes | GUI |
| Delete to trash | `ui`, MCP opt-in | yes | no | yes | yes | yes |
| `.gitignore` respected | Phase 0 (W6.1) | ? | yes | yes | ? | filters |
| Duplicate finder | `dupes` | no | no | no | no | yes |
| Age/extension breakdowns | `age`, `ext` | partial | yes | yes | no | yes |
| Cleanup suggestions w/ safety levels | `suggest` (~80 rules) | no | no | no | no | partial |
| MCP server for agents | yes (8 tools + opt-in trash) | no | no | no | no | no |
| Snapshot/diff over time | `snapshot`, `diff` | no | no | export/import | export/import | reports |
| Package managers | Phase 1 channels | cargo, brew, winget... | cargo, brew... | ... | ... | installer |

## Appendix B — benchmark methodology (for `scripts/bench/`)

1. Generate a fixture tree (e.g. 1M files across nested dirs, mixed sizes/extensions/mtimes) plus use one real large directory per OS.
2. One warmup run per tool, then N=5 interleaved (round-robin) runs; report wall min/median, CPU seconds min, peak RSS.
3. Tools: cleanmcp (release build), dua, dust, gdu — pinned versions recorded in the output.
4. Same machine, same day, AC power, AV exclusions noted; results appended to `docs/benchmarks.md` with date and machine.
5. The CI perf gate uses the fixture tree only (deterministic), on a fixed runner size, comparing against a baseline JSON committed in the repo; fail at >10% wall-time regression on scan and on the four whole-tree queries.
