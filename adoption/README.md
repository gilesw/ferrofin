# Adoption smoke test

Ferrofin supports in-place adoption of Jellyfin **10.11.8, 10.11.9, 10.11.10, 10.11.11,
12.0.0 and 12.1.0** databases. Adoption is one-way; returning to Jellyfin requires restoring
a backup. Follow the [migration procedure](../docs/INSTALL.md#migrate-an-existing-jellyfin-installation)
for a real installation.

## Supported and tested versions

All seven fixture paths below **passed** the live suite on **2026-09-16 (US/Mountain)**.
Each test booted Ferrofin on a fresh copy of a real Jellyfin database from the same library.
The 10.11.8 fixture was the original snapshot; the others were produced by booting the
corresponding Jellyfin releases on copies, as shown below.

| Source Jellyfin version | Fixture upgrade path before adoption | EF migration IDs | Generation logged by Ferrofin | Live result |
|---|---|---:|---|---|
| 10.11.8 | Original 10.11.8 snapshot | 68 | `10.11.8` | PASS |
| 10.11.9 | 10.11.8 → 10.11.9 | 68 | `10.11.8` | PASS |
| 10.11.10 | 10.11.8 → 10.11.10 | 71 | `10.11.11` | PASS |
| 10.11.11 | 10.11.8 → 10.11.11 | 71 | `10.11.11` | PASS |
| 12.0.0 | 10.11.8 → 12.0.0 | 102 | `12.0.0` | PASS |
| 12.1.0 | 10.11.8 → 12.1.0 | 104 | `12.1.0` | PASS |
| 12.1.0 | 10.11.8 → 12.0.0 → 12.1.0 | 105 | `12.1.0` | PASS |

The gate in `crates/ferrofin-db/src/database.rs` matches migration **ID sets**, not just
their counts or a version string. Releases sharing a set share the logged generation.
Both 12.1 paths are accepted because the older rating-level migration ID is optional.
Unknown or incomplete histories are refused; the listed versions do not imply support
for other 10.11.x or 12.x releases. Fresh-install databases and other upgrade routes were
not separately exercised by this live run.

The tested server image was `ferrofin:adopt121`, supplied as built from server commit
`1732f97c`, with local image ID
`sha256:0b29af8f135a83d196329a0ada9f41f0ecd7d851e98dde9092d4dd01766ca42d`.
The run used the corrected harness that reports SQLite query failures and detects
alternate-version promotions even when `repaired` is zero. All seven fixtures passed
with exit status 0. Later server changes require rerunning this matrix.

## What the live suite checks

1. The boot log names the expected generation, reports completed database migrations and
   logs no `ERROR`.
2. The read-only probes in `smoke.sh` produce the same normalised summaries as **Jellyfin
   12.1** on the same library. IDs and image byte counts are normalised; server-info,
   task, plugin, device, virtual-folder, activity-log and session lines are excluded.
3. `PRAGMA integrity_check` and `PRAGMA foreign_key_check` are clean on the adopted file,
   apart from the documented `IX_Peoples_NameLower` exception below. SQLite query failures
   are reported as failures.
4. A second boot reports no repeated repair work, including version promotions, and
   produces the same normalised probe summaries.
5. For every movie, series and episode in the source fixture, populated metadata
   remains populated after adoption, after restart, and after an explicitly requested
   library scan. Both SQLite and the HTTP item responses are checked. A scan must
   produce a new successful completion record; an idle task or an old successful
   result does not pass this check.

The metadata baseline is captured from each fixture copy **before Ferrofin starts**.
It checks overview, original title, tagline, community/critic ratings, official rating,
year, premiere date, genres, studios, tags and production locations, plus provider-key
presence, cast/crew presence and image types. Item UUIDs are normalised across SQLite
and HTTP. Items omitted by browse filtering (such as alternate versions) are also
checked through their individual detail endpoints. Losing one item's metadata fails
even if another item's metadata is added.
Fields absent in Jellyfin are optional; valid provider updates may change values.
This checks presence, not exact text, provider-ID values, cast membership or image bytes.
At each stage, the suite downloads and decodes artwork for a fixed 10% sample of
movies and a separate 10% sample of series that had posters or backdrops in Jellyfin
(rounded up). Sorting the original item UUIDs keeps the sample identical across
stages and repeat runs. Each sampled item's poster and first backdrop are checked
where present, plus one episode poster, with at most four downloads/decodes running
at once. Pillow must decode the image pixels; an image content type alone does not
pass. Artwork records, descriptions and ratings are
still checked for every original item, including items outside the artwork sample.
The source must contain at least one movie with both an overview and a provider ID,
so an empty or filename-only fixture cannot pass.

Only fixture copies are scanned; source snapshots remain untouched. All media mounts
must resolve for the scan, which can otherwise remove unavailable items. The scan
keeps the fixture's provider settings, but the container's HTTP(S) requests use an
unreachable local proxy. Provider failures therefore cannot be hidden by a later
successful download refilling lost metadata, and the check does not depend on live
API keys or quotas. This tests retention during provider failure; provider enrichment
is covered separately by the server's controlled HTTP scan tests. Its default timeout is
600 seconds; set `ADOPTION_SCAN_TIMEOUT` for larger libraries. Set
`ADOPTION_WORK_DIR` to isolate a run from previous artifacts (default: `$FIXTURES/work`).
A timeout, failed scan, failed query, malformed response or inaccessible metadata
endpoint fails the run.

The new metadata reports contain field names and aggregate failure counts, with no
titles, paths, provider-ID values or credentials. The baseline file contains item UUIDs
and presence information only and is written with owner-only permissions. Existing
smoke outputs, server logs and database copies can still contain library details;
keep those local. No reporter library data is needed to test the harness.

These checks cover the reference library and the probe summaries; they do not assert
equality of every API field or cover every possible library configuration.

It is not a CI gate: it needs a populated library and fixtures. Each generation takes
about two minutes for the boot probes, plus a complete library scan. Run it before
any change to `crates/ferrofin-db/migrations/`, the adoption gate in
`database.rs`, or the boot repairs in `adoption_repairs.rs`, and before a release.

## The fixtures are yours, not the repository's

Nothing under `adoption/` contains a database. You supply **one** Jellyfin 10.11.8 data
directory and the builder derives the rest with the official Jellyfin images:

Committed test cases use invented users, item IDs and metadata. Keep real fixture
snapshots and generated reports outside the checkout. Git ignore rules also exclude
the fixture directories, media mount scripts and generated metadata/smoke reports;
never force-add these private artifacts. Live baselines retain item UUIDs locally so
the same items can be checked across stages, but reports show only aggregate results.

```
$FIXTURES/
  jellyfin-10.11.8/            supplied: config/, data/jellyfin.db, root/, metadata/ …
  media-mounts.sh              optional: MEDIA=(-v /host/path:/container/path:ro …)
  jellyfin-10.11.9/            built: one boot of jellyfin/jellyfin:10.11.9 on 10.11.8
  jellyfin-10.11.10/           built: one boot of jellyfin/jellyfin:10.11.10 on 10.11.8
  jellyfin-10.11.11/           built: one boot of jellyfin/jellyfin:10.11.11 on 10.11.8
  jellyfin-12.0/               built: one boot of jellyfin/jellyfin:12.0
  jellyfin-12.1-from-10/       built: one boot of jellyfin/jellyfin:12.1 on 10.11.8
  jellyfin-12.1-from-12/       built: one boot of jellyfin/jellyfin:12.1 on 12.0
  oracle/smoke-jellyfin-12.1.txt  built: Jellyfin 12.1's own smoke answers
  oracle/user.txt              built: the account those answers were probed as
  work/                        scratch; a failed fixture's copy, logs and smoke output stay here
```

Take the snapshot with the server stopped, or copy the database with
`sqlite3 jellyfin.db ".backup /snapshot/data/jellyfin.db"` so the WAL is folded in. Bring
`root/` (library definitions) and `metadata/` (images) along with `data/`; without them the
library list is empty and every poster is a placeholder, which the probes will notice. The
media paths referenced by the library options must resolve inside the container, hence
`media-mounts.sh`. The probes need an administrator with a **Jellyfin Web** session in `Devices`
(the builder records which account the oracle ran as in `oracle/user.txt` and the runner
probes every fixture as that account; `--user NAME` or `ADOPTION_USER` overrides) and an
**API key** in `ApiKeys`. Credentials are read from the copy at run time and never written.
The runner also requires Python 3 and Pillow for metadata checks and image decoding
(`python3-pil` on Debian/Ubuntu). The artwork checker does not invoke FFmpeg.

```bash
adoption/build-fixtures.sh --fixtures /path/to/fixtures     # once, ~25 min, pulls 10.11.9–12.1
docker build -t ferrofin:bench .                            # the commit under test
adoption/run.sh --fixtures /path/to/fixtures --image ferrofin:bench
adoption/run.sh --fixtures … --only jellyfin-12.1-from-10   # one generation
```

Output is one line per generation:

```
PASS  10.11.8   jellyfin-10.11.8
PASS  10.11.8   jellyfin-10.11.9
PASS  10.11.11  jellyfin-10.11.10
PASS  10.11.11  jellyfin-10.11.11
PASS  12.0.0    jellyfin-12.0
PASS  12.1.0    jellyfin-12.1-from-10
PASS  12.1.0    jellyfin-12.1-from-12
```

The second column is the generation the gate matched — the id *set*, so 10.11.9 reports
`10.11.8` and 10.11.10 reports `10.11.11`; `run.sh` knows which is expected for which fixture.
A `FAIL` line names every check that failed and points at the diff; the copy is kept under
`work/` with `<name>.server.log`, `<name>.smoke.txt`, `<name>.smoke2.txt` and the metadata
presence baseline `<name>.metadata.json` beside it. The per-stage
`<name>.metadata-after-{adoption,restart,scan}.log` files record the metadata results.

## Tests

`adoption/tests/adoption.bats` covers the harness itself without docker or fixtures: the
checks in `lib.sh` (log parsing, smoke normalisation and comparison, the SQLite checks, the
second-boot repair detection, the credential picker) run against canned logs, smoke outputs
and throwaway SQLite files, and the two entry points are exercised for their refusals and the
missing-fixture path. `tests/test_metadata.py` exercises the metadata checks using
invented library rows and a local HTTP server: partial/total metadata loss, missing
items, empty baselines, optional fields, valid updates, WAL visibility, HTTP failures,
batched API reads, scan completion/failure/timeout handling, reproducible artwork
sampling, corrupt image responses and description/rating loss. Bats invokes these
Python tests, so CI runs them too. CI runs the Bats tests with `bats adoption/tests`
next to `scripts/tests`; locally `mise exec bats@latest -- bats adoption/tests` or
a system `bats` works.

The 2026-09-16 validation also passed all **23 adoption harness tests** and **13 script
tests** (36 total), plus ShellCheck. The database crate passed **121 tests**, including
12.0 adoption, both 12.1 migration histories, incomplete-history rejection and schema
conformance; one performance test was ignored. These automated checks complement the
live matrix above; CI does not run the real-library fixture suite.

## Adding a generation

When Jellyfin ships a release with new `__EFMigrationsHistory` ids: add a builder step that
boots that image on the right parent fixture, add a row to `FIXTURE_TABLE` in `run.sh`, refresh
the oracle if that release changes answers (delete `oracle/` and rebuild), then teach the gate
(`JELLYFIN_GENERATIONS` in `crates/ferrofin-db/src/database.rs`). The oracle is always the
newest supported Jellyfin: parity is measured against the current release, not the one the
database came from.

## Why the checks are what they are

- **Generation in the log**, not just "it booted": a database adopted as the wrong generation
  baselines the wrong migrations and runs the wrong one-shot repairs (the 10.11.11 gate once
  skipped the playlist import for exactly that reason).
- **Jellyfin's answers as the oracle**: counts and first items are what a client shows; a
  migration that loses rows or hides them is invisible to a green test suite and obvious here.
- **Second boot**: every repair is keyed in `FerrofinMeta` and must not run twice; answers that
  move between two boots mean a repair is not idempotent.
- **`integrity_check` exempts `IX_Peoples_NameLower`**: it indexes `lower("Name")`, and a host
  `sqlite3` built with ICU lower-cases non-ASCII names differently from the SQLite inside
  Jellyfin and Ferrofin, so it reports rows "missing from index" on a file both servers agree
  with. Nothing else is exempt.

## Metadata gate validation

All **seven fixture paths passed the expanded metadata and artwork checks** on
**2026-09-29**, using local fixtures and Pillow for artwork decoding. Each baseline
contained **318 movies, 126 series and 8,878 episodes** (9,322 items). Database and HTTP
checks passed after adoption, restart and a completed scan. Each stage compared
against the original Jellyfin baseline and decoded artwork for the fixed 10% sample.

| Fixture | Adoption | Restart | Completed scan |
|---|---|---|---|
| `jellyfin-10.11.8` | PASS | PASS | PASS |
| `jellyfin-10.11.9` | PASS | PASS | PASS |
| `jellyfin-10.11.10` | PASS | PASS | PASS |
| `jellyfin-10.11.11` | PASS | PASS | PASS |
| `jellyfin-12.0` | PASS | PASS | PASS |
| `jellyfin-12.1-from-10` | PASS | PASS | PASS |
| `jellyfin-12.1-from-12` | PASS | PASS | PASS |

The local correctness image was `ferrofin:adoption-metadata-23`, image ID
`sha256:d85ab7a9861c8a4185ee690047bc12a6c78ee3e667eebee6bfaa4e6090d77a91`.
It packaged the debug server built from `ded26d35` with its host runtime libraries,
using the existing `ferrofin:bench` image for web assets and FFmpeg. This was a
correctness run, not a release performance measurement. The runs used the provider
outage configuration above, an isolated `ADOPTION_WORK_DIR`, and
`ADOPTION_SCAN_TIMEOUT=1200`.

All **26 Bats harness tests**, including **26 Python metadata tests**, passed, along
with ShellCheck and shell syntax checks. A separate native-server test checked a
synthetic movie after scan, restart and another scan, then deliberately cleared its
overview and confirmed that both the database and HTTP checks detected the loss.

The complete matrix was rerun on fresh copies after replacing the host FFmpeg
dependency with Pillow, using the same server image. The sample checks posters and
backdrops for 32 movies and 13 series, plus one episode poster.
The regression tests include explicit database and post-scan API checks for missing
descriptions and ratings, corrupt images, stable sampling and decoder failures.
Committed test data is synthetic; real snapshots and reports remain local and
excluded from Git.
