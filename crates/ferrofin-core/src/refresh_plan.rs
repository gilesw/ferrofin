//! The per-item refresh decision of a library scan: which providers run for
//! an item, and whether the item is saved afterwards.
//!
//! A port of the decision half of `MetadataService.RefreshMetadata`
//! (`MediaBrowser.Providers/Manager/MetadataService.cs:89-270`), with its
//! provider selection (`GetProviders`, `:647-715`), its remote image selection
//! (`GetNonLocalImageProviders`, `:717-749`) and the change monitors the
//! scan's providers carry: `ProbeProvider.HasChanged`
//! (`MediaInfo/ProbeProvider.cs:127-201`), `BaseNfoProvider.HasChanged`
//! (`XbmcMetadata/Providers/BaseNfoProvider.cs:67-80`) and
//! `BaseXmlProvider.HasChanged` (`LocalMetadata/BaseXmlProvider.cs:97-107`).
//! `BaseItem.RequiresRefresh` (`Entities/BaseItem.cs:1721-1731`) and its
//! `Folder` override (`Entities/Folder.cs:213-223`) decide `requiresRefresh`.
//!
//! Everything here is pure: the scan gathers the stored row's dates, the
//! filesystem facts and the options, and acts on the answer. Upstream has no
//! Ferrofin-style backfill rule; the scan passes its own (kept by owner
//! decision D2 of `PLAN_SCAN_CHANGE_DETECTION`) in as an extra trigger for
//! the remote metadata providers only.

use chrono::{DateTime, TimeDelta, Utc};
use ferrofin_model::configuration::LibraryOptions;
use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};

/// `BaseItemExtensions.HasChanged` (`BaseItemExtensions.cs:125-130`): a file
/// has changed when `|DateModified − mtime|` exceeds one second.
const FILE_CHANGE_TOLERANCE_MS: i64 = 1_000;

/// `BaseNfoProvider.HasChanged` (`BaseNfoProvider.cs:77-78`): "1 minute
/// tolerance to avoid detecting our own file writes" — an NFO is newer only
/// when it was written more than a minute after the item was last saved.
const NFO_SAVE_TOLERANCE_MS: i64 = 60_000;

/// `BaseXmlProvider.HasChanged` (`BaseXmlProvider.cs:106`) compares the XML
/// sidecar's mtime with `DateLastSaved` with no tolerance at all.
const XML_SAVE_TOLERANCE_MS: i64 = 0;

/// The options one refresh runs with: `MetadataRefreshOptions` plus
/// upstream's `ForceSave`, which the trait-level options do not carry.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RefreshRequest<'a> {
    /// The refresh modes and replace flags.
    pub options: &'a MetadataRefreshOptions,
    /// `MetadataRefreshOptions.ForceSave`: save the item whatever changed.
    pub force_save: bool,
}

/// What the scan read of an item's stored row, as far as the decision needs
/// it. A new item has none (`plan_item_refresh(None, …)`), which reads the
/// same as a row with every date unset, as upstream's freshly created item
/// has `DateTime.MinValue` in each.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct StoredState {
    /// `DateLastRefreshed`; `None` means the item was never refreshed.
    pub date_last_refreshed: Option<DateTime<Utc>>,
    /// `DateLastSaved`, which the NFO/XML change monitors compare against.
    pub date_last_saved: Option<DateTime<Utc>>,
    /// `DateModified`: the file (or folder) mtime the row was saved with.
    pub date_modified: Option<DateTime<Utc>>,
    /// `RunTimeTicks`; with `total_bitrate`, what `IsMissingMediaInfo` reads.
    pub run_time_ticks: Option<i64>,
    /// `TotalBitrate`.
    pub total_bitrate: Option<i64>,
    /// `IsVirtualItem`: a virtual item is never missing media info.
    pub is_virtual_item: bool,
    /// `IsLocked`: only the forced providers (the probe) and the local image
    /// validation run for a locked item.
    pub is_locked: bool,
}

/// Which prober handles the item, if any (`ProbeProvider.HasChanged`'s
/// `item as Video` / `item is Audio` split).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeKind {
    /// The item is never probed (a folder, a photo, a book…).
    None,
    /// A video. `file_or_iso` is `VideoType == VideoFile || VideoType == Iso`:
    /// only those compare the file's mtime (a disc folder does not).
    Video {
        /// Whether the mtime arm applies.
        file_or_iso: bool,
    },
    /// An audio item.
    Audio,
}

/// The local metadata sidecar found for the item, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LocalMetadataFile {
    /// The sidecar's mtime.
    pub mtime: DateTime<Utc>,
    /// Which reader it belongs to (which tolerance applies).
    pub format: LocalMetadataFormat,
}

/// The local metadata reader a sidecar belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalMetadataFormat {
    /// A Kodi/XBMC `.nfo` (`BaseNfoProvider`).
    Nfo,
    /// A Jellyfin `.xml` (`BaseXmlProvider`). Upstream's XML readers serve
    /// box sets and playlists only (`BoxSetXmlProvider`,
    /// `PlaylistXmlProvider`), which the library scan does not resolve, so
    /// the scan never passes one; the arm is ported for their refresh.
    #[cfg_attr(not(test), allow(dead_code))]
    Xml,
}

/// What the filesystem says about the item now.
// Independent facts about the file, one flag each.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileFacts {
    /// The mtime of the item's path, `None` when it has no path or the stat
    /// failed (`info.Exists` false).
    pub mtime: Option<DateTime<Utc>>,
    /// The prober for this item.
    pub probe: ProbeKind,
    /// The external subtitle **or** audio files next to a video differ from
    /// the ones its stored streams came from (`SubtitleFiles`/`AudioFiles`).
    pub sidecars_changed: bool,
    /// The external lyric files of an audio item differ from the stored
    /// ones (`LyricFiles`).
    pub lyrics_changed: bool,
    /// The NFO/XML sidecar the local reader would read.
    pub local_metadata: Option<LocalMetadataFile>,
    /// `Folder.SupportsCumulativeRunTimeTicks` (a music album or artist).
    pub supports_cumulative_run_time: bool,
    /// `IsShortcut`: a `.strm` file, whose target is not probed from disk.
    pub is_shortcut: bool,
    /// `IsFileProtocol`: the path is a local file, not a stream URL.
    pub is_file_protocol: bool,
    /// `Video.IsPlaceHolder`: a disc stub standing in for offline media.
    pub is_placeholder: bool,
}

/// Which remote image providers run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageFetch {
    /// None: only the local images are validated.
    None,
    /// The remote providers run and fill the image types still missing.
    MissingOnly,
    /// The remote providers run and replace every image
    /// (`ReplaceAllImages`).
    All,
}

/// What one refresh of one item runs.
// One flag per provider stage upstream decides separately; they are
// independent, not states of one machine.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ItemRefreshPlan {
    /// `isFirstRefresh`: the item has never been refreshed.
    pub is_first_refresh: bool,
    /// `requiresRefresh`: the refresh interval elapsed, the file changed,
    /// or a cumulative-runtime folder has no runtime.
    pub requires_refresh: bool,
    /// Run the probe (`ProbeProvider`).
    pub probe: bool,
    /// Run the local metadata readers (NFO, and the embedded readers).
    pub local_metadata: bool,
    /// A local change monitor fired (the NFO is newer than the last save,
    /// or the probe's), rather than the local readers running only because
    /// the remote providers do.
    pub local_monitor_fired: bool,
    /// Run the remote metadata providers.
    pub remote_metadata: bool,
    /// The remote metadata providers run because upstream runs all of them
    /// (`runAllProviders`), not only because of the scan's backfill rule.
    pub run_all_providers: bool,
    /// Which remote image providers run.
    pub remote_images: ImageFetch,
}

impl ItemRefreshPlan {
    /// A plan that runs nothing: no probe, no reader, no provider.
    pub(crate) const IDLE: Self = Self {
        is_first_refresh: false,
        requires_refresh: false,
        probe: false,
        local_metadata: false,
        local_monitor_fired: false,
        remote_metadata: false,
        run_all_providers: false,
        remote_images: ImageFetch::None,
    };
}

/// What a refresh pass did, as far as the save rule needs it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PassOutcome {
    /// Something the save would persist differs from what is stored
    /// (upstream's `updateType > ItemUpdateType.None`).
    pub changed: bool,
    /// A provider in this pass failed (`RefreshResult.Failures > 0`, or a
    /// local image validation that threw).
    pub failed: bool,
}

/// Whether and how the item is saved after the pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SaveDecision {
    /// Write the item (the row and everything it owns).
    pub save: bool,
    /// Set `DateLastRefreshed` to now on the saved row.
    pub stamp_refreshed: bool,
}

/// `|a − b| > tolerance`.
fn drifted(a: DateTime<Utc>, b: DateTime<Utc>, tolerance_ms: i64) -> bool {
    (a - b).abs() > TimeDelta::milliseconds(tolerance_ms)
}

/// `BaseItem.HasChanged(asOf)` against a stored `DateModified` that may be
/// unset — upstream's `DateTime.MinValue`, which every real mtime differs
/// from by far more than a second.
fn file_changed(stored: Option<DateTime<Utc>>, mtime: DateTime<Utc>) -> bool {
    stored.is_none_or(|stored| drifted(stored, mtime, FILE_CHANGE_TOLERANCE_MS))
}

/// TODO(parity, open work item — NOT an accepted divergence): two more
/// upstream overrides are not ported. `CollectionFolder.RequiresRefresh`
/// (`CollectionFolder.cs:206-230`) also fires when the library's physical
/// locations changed, and `MusicArtist.RequiresRefresh` (`MusicArtist.cs:
/// 186-198`) when an accessed-by-name artist's rebased path moved. The scan
/// plans neither kind today (collection folders and by-name artists are
/// written outside the per-item walk); porting them means feeding those two
/// facts in `FileFacts` when those items go through this decision.
///
/// `BaseItem.RequiresRefresh` + the `Folder` override: the file changed
/// (never for a path-less item or an unset `DateModified`), or a folder that
/// sums its children's runtime has none.
fn item_requires_refresh(stored: &StoredState, fs: &FileFacts) -> bool {
    let changed = matches!(
        (stored.date_modified, fs.mtime),
        (Some(saved), Some(now)) if drifted(saved, now, FILE_CHANGE_TOLERANCE_MS)
    );
    changed || (fs.supports_cumulative_run_time && stored.run_time_ticks.is_none())
}

/// `ProbeProvider.HasChanged` + `IsMissingMediaInfo`.
fn probe_changed(stored: &StoredState, fs: &FileFacts) -> bool {
    let (checks_mtime, is_video) = match fs.probe {
        ProbeKind::None => return false,
        ProbeKind::Video { file_or_iso } => (file_or_iso, true),
        ProbeKind::Audio => (true, false),
    };
    if checks_mtime
        && fs
            .mtime
            .is_some_and(|mtime| file_changed(stored.date_modified, mtime))
    {
        return true;
    }
    // `IsMissingMediaInfo` (`ProbeProvider.cs:179-195`). A video's
    // `IsCompleteMedia` is false only for a channel livestream or an active
    // recording, neither of which a library scan resolves.
    let missing_media_info = stored.run_time_ticks.is_none()
        && stored.total_bitrate.is_none()
        && !stored.is_virtual_item
        && !fs.is_shortcut
        && fs.is_file_protocol
        && !(is_video && fs.is_placeholder);
    missing_media_info || (is_video && fs.sidecars_changed) || (!is_video && fs.lyrics_changed)
}

/// `BaseNfoProvider.HasChanged` / `BaseXmlProvider.HasChanged`: the sidecar
/// was written after the item was last saved (an unset `DateLastSaved` is
/// `DateTime.MinValue`, so any sidecar is newer).
fn local_metadata_changed(stored: &StoredState, fs: &FileFacts) -> bool {
    let Some(file) = fs.local_metadata else {
        return false;
    };
    let tolerance = match file.format {
        LocalMetadataFormat::Nfo => NFO_SAVE_TOLERANCE_MS,
        LocalMetadataFormat::Xml => XML_SAVE_TOLERANCE_MS,
    };
    stored
        .date_last_saved
        .is_none_or(|saved| file.mtime - saved > TimeDelta::milliseconds(tolerance))
}

/// Decides which providers run for one item.
///
/// - `isFirstRefresh`: `DateLastRefreshed` unset.
/// - `requiresRefresh`: `AutomaticRefreshIntervalDays > 0` and that many
///   days since `DateLastRefreshed`; else, when the metadata mode is not
///   `None`, [`item_requires_refresh`].
/// - `GetProviders`: every provider runs when replacing all metadata, on a
///   full refresh, or on a first/required refresh in `Default` mode or above;
///   otherwise only the providers whose change monitor fired, plus every
///   local reader when any of them did. No remote provider has a change
///   monitor, so a remote provider runs only in the run-all case — or, in
///   the scan, when `backfill` says so (never for a locked item, and never
///   in `ValidationOnly` or `None` mode). A locked item runs the probe only:
///   no local reader and no remote provider.
/// - `GetNonLocalImageProviders`: the remote image providers run on an
///   image full refresh or for an item never refreshed, and never for a
///   locked item outside an image full refresh (`CanRefreshImages`).
pub(crate) fn plan_item_refresh(
    stored: Option<&StoredState>,
    fs: &FileFacts,
    request: &RefreshRequest<'_>,
    library: Option<&LibraryOptions>,
    now: DateTime<Utc>,
    backfill: bool,
) -> ItemRefreshPlan {
    let unset = StoredState::default();
    let stored = stored.unwrap_or(&unset);
    let options = request.options;
    let mode = options.metadata_refresh_mode;
    let is_first_refresh = stored.date_last_refreshed.is_none();
    let interval_days = library.map_or(0, |l| l.automatic_refresh_interval_days);
    let mut requires_refresh = interval_days > 0
        && stored
            .date_last_refreshed
            .is_none_or(|last| now - last >= TimeDelta::days(i64::from(interval_days)));
    if !requires_refresh && mode != MetadataRefreshMode::None {
        requires_refresh = item_requires_refresh(stored, fs);
    }

    let at_least_default = matches!(
        mode,
        MetadataRefreshMode::Default | MetadataRefreshMode::FullRefresh
    );
    let (probe, local_metadata, local_monitor_fired, remote_metadata, run_all_providers) =
        if mode == MetadataRefreshMode::None {
            (false, false, false, false, false)
        } else {
            let run_all = options.replace_all_metadata
                || mode == MetadataRefreshMode::FullRefresh
                || (is_first_refresh && at_least_default)
                || (requires_refresh && at_least_default);
            let probe_changed = probe_changed(stored, fs);
            let local_changed = local_metadata_changed(stored, fs);
            let probe = fs.probe != ProbeKind::None && (run_all || probe_changed);
            // `CanRefreshMetadata` (`ProviderManager.cs:588-592`): a locked
            // item runs local and forced providers only — and of those,
            // `RefreshWithProviders` returns on `item.IsLocked`
            // (`MetadataService.cs:785-788`) after the pre-refresh ones (the
            // forced probe) and BEFORE the local readers, so an NFO is never
            // read for a locked item either. Its local images are validated
            // outside this decision (`MetadataService.cs:123-143`).
            let remote = !stored.is_locked && (run_all || (backfill && at_least_default));
            // "If any provider reports a change, always run local ones as
            // well" (`MetadataService.cs:689-693`): the backfill counts as a
            // remote provider reporting a change, so the local readers run
            // with it and the merge keeps what they supply (an NFO's overview
            // and cast are not traded for the remote ones). Upstream would
            // also run the custom providers (the probe) then; the backfill
            // is Ferrofin's own trigger and asks nothing of the file, so it
            // does not re-probe.
            let local = !stored.is_locked && (run_all || local_changed || probe_changed || remote);
            (
                probe,
                local,
                !stored.is_locked && (local_changed || probe_changed),
                remote,
                run_all && !stored.is_locked,
            )
        };

    let image_mode = options.image_refresh_mode;
    let remote_images = if !matches!(
        image_mode,
        MetadataRefreshMode::Default | MetadataRefreshMode::FullRefresh
    ) || (stored.is_locked && image_mode != MetadataRefreshMode::FullRefresh)
        || !(image_mode == MetadataRefreshMode::FullRefresh || is_first_refresh)
    {
        ImageFetch::None
    } else if options.replace_all_images {
        ImageFetch::All
    } else {
        ImageFetch::MissingOnly
    };

    ItemRefreshPlan {
        is_first_refresh,
        requires_refresh,
        probe,
        local_metadata,
        local_monitor_fired,
        remote_metadata,
        run_all_providers,
        remote_images,
    }
}

/// The tail of `RefreshMetadata`: the `DateLastRefreshed` stamp and
/// `SaveInternal`'s save rule (`MetadataService.cs:216-262`).
///
/// A pass that attempted a fetch (either mode above `ValidationOnly`) and had
/// no provider failure stamps `DateLastRefreshed`; a provider that answered
/// with nothing is not a failure. The item is saved when forced, when the
/// pass changed something, on a first or required refresh, when replacing all
/// metadata, or on a full refresh whose only change is that stamp — without
/// it a full refresh that found nothing would repeat the same queries
/// forever.
pub(crate) fn decide_save(
    plan: ItemRefreshPlan,
    request: &RefreshRequest<'_>,
    pass: PassOutcome,
) -> SaveDecision {
    let options = request.options;
    let fetches = |mode: MetadataRefreshMode| {
        matches!(
            mode,
            MetadataRefreshMode::Default | MetadataRefreshMode::FullRefresh
        )
    };
    let attempted_fetch =
        fetches(options.metadata_refresh_mode) || fetches(options.image_refresh_mode);
    let stamp_refreshed = !pass.failed && attempted_fetch;
    let stamp_needs_saving = stamp_refreshed
        && (options.metadata_refresh_mode == MetadataRefreshMode::FullRefresh
            || options.image_refresh_mode == MetadataRefreshMode::FullRefresh);
    let save = request.force_save
        || pass.changed
        || plan.is_first_refresh
        || options.replace_all_metadata
        || plan.requires_refresh
        || stamp_needs_saving;
    SaveDecision {
        save,
        stamp_refreshed,
    }
}

#[cfg(test)]
mod tests;
