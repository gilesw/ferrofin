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
