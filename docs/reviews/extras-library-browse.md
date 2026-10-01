# Extras library browse fix (#32)

## Reference and baseline

Baseline: Ferrofin `b0e083c6` (v1.3.0 plus #27).
Reference source: Jellyfin tag `v12.1`, commit
`ee91c75e777da41a9c4f4855e70adc604fbf2ef8`.

The source comparison establishes these requirements:

- `BaseItem.RefreshExtras` sets `OwnerId` and clears `ParentId`.
- `BaseItem.GetTopParent` walks physical parents. Parentless extras therefore
  have no `TopParentId` when `ItemPersistenceService` saves them.
- `BaseItem.GetAncestorIds` includes collection folders. `LibraryManager.
  GetCollectionFolders` follows `OwnerId` when a parent is absent, so retaining
  the owning collection folder in the ancestor table is appropriate.
- `LibraryManager.FindExtras` resolves candidates through `ResolvePath`, which
  applies the ignore rules. `sample.mkv` and `*.sample.mkv` are ignored;
  `*-sample.mkv`, `*_sample.mkv`, and files in `samples/` can be owned extras.
  A blanket exclusion of `ExtraType.Sample` would discard supported media.
- Extras naming rules recognize `Extras/` as `Unknown` (0), including files
  whose names look like trailers inside that folder. Rule order is significant.

The regression tests exercise unfiltered direct and recursive browsing, owner
lookup, locked existing rows, and the distinction between ignored sample names
and supported Sample extras. These are source-derived expectations. A live
Jellyfin 12.1 fixture comparison remains a separate validation requirement.

## Execution record

### Phase 1: regression coverage

Added `library_scan_extras.rs`. The baseline is expected to fail the ownership,
existing-row repair, and ignored-path tests. No production code changes in this
phase.

### Runtime prerequisites

The current tool environment cannot access `/var/run/docker.sock`; passwordless
sudo is unavailable and no .NET SDK is installed. Existing adoption fixtures are
present under `/tmp`. Live Jellyfin and container-based adoption checks require
an accessible runtime. This limitation is not a passed validation result.

### Phase 2: ownership and lifecycle

Extras now have no physical parent or top parent. Their collection ancestor is
retained for owner-library policy and progress accounting. Native recursive
library browsing now uses the library's top-parent scope, matching the adopted
library path. General extras-query predicates are unchanged.

Pruning reads parentless extras through the owner's library, with index seeks
on the owner's TopParentId and the extra's OwnerId. Scoped scans seek the
extra's Path and check its owner by primary key, keeping watcher work bounded. Rows returned through both legacy and owned membership
are deduplicated. Owner deletion already follows OwnerId and needs no change.

The baseline's three regression tests failed as expected. After the ownership
fix, four ownership/lifecycle integration tests pass, including repair of a
locked unchanged extra and stable DateLastSaved on a second scan. Existing
row comparison and structural overlays already perform the required repair;
no new refresh trigger or provider pass is needed. Ignore discovery remains
for phase 3. An EXPLAIN test guards the new pruning query's index seeks.

### Phase 3: discovery and owner eligibility

The planner filters media entries through the existing ignore rules. Location
availability is recorded from the raw listing first, so exclusions cannot make
a mounted location look unavailable. Provider filesystem reads remain raw for
artwork and sidecars. Full and scoped discovery share the same planner.

Movie-folder recognition now uses VideoListResolver's version grouping and
MovieResolver's sample regex. A root/mixed folder or a directory with ordinary
subfolders does not supply an extras owner. This follows
BaseItem.SearchesContainingFolderForExtras; it also sets IsInMixedFolder and
uses the filename for movies that do not have their own folder.

Seven integration tests pass, including scoped ignored-file discovery with NFO
preservation and nested/multiple-movie ownership. A naming test covers the
reference sample regex's word boundaries.

### Live Jellyfin 12.1 reference

The Docker limitation was worked around by installing a temporary .NET 10 SDK
under `/tmp` and building the exact `v12.1` source tag. The resulting server
reports 12.1.0. A disposable server on port 18132 scanned generated one-second
media with internet metadata disabled. Local results are under
`/tmp/ferrofin-extras-oracle/` (reference.json and extended.json).

Observed in a single-movie directory:

| Path relative to the movie folder | Stored kind / ExtraType |
| --- | --- |
| `Solo.mkv` | Movie / null |
| `Extras/Deleted.Scenes.avi` | Video / 0 |
| `Solo-trailer.mkv` | Trailer / 2 |
| `Solo-sample.mkv` | Video / 7 |
| `Solo_sample.mkv` | Video / 7 |
| `samples/clip.mkv` | Video / 7 |
| `sample.mkv`, `Solo.sample.mkv` | Absent |

All retained extras have an owner and null ParentId/TopParentId. Hidden files
and ignored directories were absent. In a directory containing a nested release,
Jellyfin marked the outer movie as mixed and did not create its extras; the
nested movie retained its own suffix sample. These observations match the new
ownership tests. Jellyfin also preserves physical folder rows in that layout;
Ferrofin's existing flattening of ordinary movie subfolders is a separate
hierarchy mismatch, so whole-library folder counts are not asserted equal.

Phase 3 review caught owner registration treating each stacked part and version
as an unrelated movie. Registration now uses the grouped title returned by the
naming resolver. Generic extras choose the primary; named extras choose the
longest version prefix ending at a delimiter, as `Video.GetOwnerIdForExtra` does.
Adopted Video identities are reused for owners even outside a scoped refresh.
All nine extras integration tests pass (full/scoped versions, stacks, adopted
identities included). The second independent review approved this phase.

## Phase 4 — reconcile confirmed exclusions

The planner records excluded entries only after successful directory listings,
and records extra candidates whose listed containing folder has no eligible
owner. Pruning considers these paths even when their files still exist. Scope,
unlisted/unavailable locations, cancellation, library roots, and retained-child
cascade guards still apply. Membership uses path ancestors in a hash set, so it
does not multiply existing rows by the number of excluded paths.

The twelve extras tests pass, including exact-path and folder cleanup,
ownerless legacy rows, retained Sample extras, failed listings, missing and empty
mounts, cancellation after planning, and a stable second scan. Repairing a locked
extra retains its ID, overview, artwork row, provider ID, and played state.
Upgrade instructions now describe recovery with one normal scan. No schema
migration or file deletion is involved.
