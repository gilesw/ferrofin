//! The metadata merge — port of `MetadataService.MergeData` /
//! `MergeBaseItemData` (`MediaBrowser.Providers/Manager/MetadataService.cs`,
//! upstream master `208c278b75`, `:1141-1515`) and the per-kind overrides in
//! the `*MetadataService` classes.
//!
//! One function, [`merge_data`], decides for every provider-owned field
//! whether `source` replaces `target`, fills it only where it is empty, or is
//! unioned with it. Upstream's refresh modes are the two calls
//! `RefreshWithProviders` (`:897-915`) makes with it:
//!
//! - **Default** (a library scan): `merge_data(existing → provider, fill)`
//!   then `merge_data(provider → item, replace)`; provider values win and
//!   the stored values fill whatever the providers did not return.
//! - **`FullRefresh`** ("Search for missing metadata"): the second call with
//!   `replace_data = false` — stored values stay, providers fill gaps.
//! - **`ReplaceAll`** ("Replace all metadata" with `RemoveOldMetadata`): only
//!   the second call with `replace_data = true` — the provider result
//!   replaces the row and clears what it did not return.
//!
//! [`BaseItemEntity`] keeps several of upstream's properties in the `Data`
//! JSON blob (`RemoteTrailers`, `Status`, `AirDays`, …); those keys get their
//! upstream rule here too, and every other `Data` key is merged key-wise
//! (never replaced wholesale).
//!
//! The upstream properties Ferrofin has no storage for are not merged:
//! `HomePageUrl` and a person's `SortOrder`. `LockedFields` lives in its own
//! table (`BaseItemMetadataFields`), so it rides on [`MetadataResult`].
//!
//! It lives here, beside the providers, as upstream's lives in
//! `MediaBrowser.Providers`: the library scan (`ferrofin-core`) and the
//! single-item refresh (`provider_manager`) both depend on this crate, so both
//! can merge through the one rule set.

use std::collections::HashSet;

use ferrofin_db::entities::base_items::{BaseItemEntity, PeopleEntity};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::MetadataField;
use serde_json::{Map, Value};

/// A `Data` column value's JSON object; `NULL`, empty and malformed payloads
/// (and non-object JSON) are an empty object.
fn parse_data(data: Option<&str>) -> Map<String, Value> {
    match data.and_then(|d| serde_json::from_str::<Value>(d).ok()) {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// Upstream's `MetadataResult<T>`: an item plus the parts of a provider
/// result that are not columns of its row.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MetadataResult {
    /// The item row.
    pub item: BaseItemEntity,
    /// The credited people, when the result carries any (`null` upstream
    /// means "no people", which a later merge may fill).
    pub people: Option<Vec<PeopleEntity>>,
    /// The external ids (`BaseItemProviders`), as `(provider, value)` pairs.
    pub provider_ids: Vec<(String, String)>,
    /// The item's `LockedFields` (`BaseItemMetadataFields`), which the
    /// metadata-settings half of the merge unions.
    pub locked_fields: Vec<MetadataField>,
}

impl MetadataResult {
    /// A result holding only an item row.
    #[must_use]
    pub fn of(item: BaseItemEntity) -> Self {
        Self {
            item,
            people: None,
            provider_ids: Vec::new(),
            locked_fields: Vec::new(),
        }
    }
}

/// Merges `source` into `target` — port of `MergeData`.
///
/// `replace_data` picks replace (`true`) or fill-missing (`false`) for every
/// scalar field, and replace or union for the list fields that union
/// (`Studios`, `Tags`, `ProductionLocations`, `AlbumArtists`, `Artists`,
/// `RemoteTrailers`). A field in `locked_fields` is never touched (upstream
/// checks exactly `Name`, `Genres`, `OfficialRating`, `Overview`, `Cast`,
/// `Runtime`, `Studios`, `Tags` and `ProductionLocations`).
///
/// `merge_metadata_settings` additionally carries `IsLocked`,
/// `LockedFields` (a union), `DateCreated`, `DateModified` and the preferred
/// metadata language/country — upstream passes it on the provider → item
/// merge only.
pub fn merge_data(
    source: &MetadataResult,
    target: &mut MetadataResult,
    locked_fields: &[MetadataField],
    replace_data: bool,
    merge_metadata_settings: bool,
) {
    let unlocked = |field: MetadataField| !locked_fields.contains(&field);
    merge_base_item(
        &source.item,
        &mut target.item,
        &unlocked,
        replace_data,
        merge_metadata_settings,
    );
    if unlocked(MetadataField::Cast) {
        merge_people_field(source.people.as_deref(), &mut target.people, replace_data);
    }
    if merge_metadata_settings {
        // `if (target.LockedFields.Length == 0) target.LockedFields =
        // source.LockedFields; else target.LockedFields =
        // target.LockedFields.Concat(source.LockedFields).Distinct()`
        // (`MetadataService.cs:1372-1379`).
        for field in &source.locked_fields {
            if !target.locked_fields.contains(field) {
                target.locked_fields.push(*field);
            }
        }
    }
    target.provider_ids =
        merge_provider_ids(&source.provider_ids, &target.provider_ids, replace_data);
    merge_kind_specific(
        &source.item,
        &mut target.item,
        replace_data,
        merge_metadata_settings,
    );
}

/// Every field a `LockedFields` set can name (`MetadataField.cs`). What a
/// caller treats as locked when an item's stored set could not be read, so a
/// read failure never lets a write overwrite a field the user locked.
pub const ALL_LOCKABLE_FIELDS: &[MetadataField] = &MetadataField::ALL;

/// Puts `stored`'s value back on `row` for every field in `locked_fields`
/// — the columns `merge_data` never touches for a locked field. For an
/// applier that writes onto the stored row directly instead of merging (the
/// single-item refresh until it moves onto [`merge_data`]), this gives the
/// same result as the merge's lock check. `Cast` has no column: an applier
/// that writes people must check it itself.
pub fn keep_locked_fields(
    stored: &BaseItemEntity,
    row: &mut BaseItemEntity,
    locked_fields: &[MetadataField],
) {
    for field in locked_fields {
        match field {
            MetadataField::Name => {
                row.name.clone_from(&stored.name);
                // The sort key is derived from the name.
                row.sort_name.clone_from(&stored.sort_name);
            }
            MetadataField::Genres => row.genres.clone_from(&stored.genres),
            MetadataField::OfficialRating => {
                row.official_rating.clone_from(&stored.official_rating);
            }
            MetadataField::Overview => row.overview.clone_from(&stored.overview),
            MetadataField::Runtime => row.run_time_ticks = stored.run_time_ticks,
            MetadataField::Studios => row.studios.clone_from(&stored.studios),
            MetadataField::Tags => row.tags.clone_from(&stored.tags),
            MetadataField::ProductionLocations => {
                row.production_locations
                    .clone_from(&stored.production_locations);
            }
            MetadataField::Cast => {}
        }
    }
}

/// Whether a string field is empty in upstream's `string.IsNullOrEmpty`
/// sense.
fn is_empty(value: Option<&str>) -> bool {
    value.is_none_or(str::is_empty)
}

/// `string.IsNullOrWhiteSpace`.
fn is_blank(value: Option<&str>) -> bool {
    value.is_none_or(|v| v.trim().is_empty())
}

/// `if (replaceData || string.IsNullOrEmpty(target.X)) target.X = source.X;`
fn text(source: Option<&String>, target: &mut Option<String>, replace: bool) {
    if replace || is_empty(target.as_deref()) {
        target.clone_from(&source.cloned());
    }
}

/// `if (replaceData || !target.X.HasValue) target.X = source.X;`
fn value<T: Clone>(source: Option<&T>, target: &mut Option<T>, replace: bool) {
    if replace || target.is_none() {
        *target = source.cloned();
    }
}

/// The values of a `|`-joined list column, as upstream's string array.
fn list(value: Option<&str>) -> Vec<&str> {
    value
        .unwrap_or_default()
        .split('|')
        .filter(|v| !v.is_empty())
        .collect()
}

/// `target.Concat(source).Distinct(StringComparer.OrdinalIgnoreCase)`.
fn union(target: Option<&str>, source: Option<&str>) -> Option<String> {
    let mut seen = HashSet::new();
    let values: Vec<&str> = list(target)
        .into_iter()
        .chain(list(source))
        .filter(|v| seen.insert(ferrofin_util::string_extensions::upper_invariant(v)))
        .collect();
    (!values.is_empty()).then(|| values.join("|"))
}

/// The replace-or-union rule of `Studios`, `Tags`, `ProductionLocations`
/// (`:1271-1305`): replace (possibly with nothing) on `replaceData` or an
/// empty target, otherwise union.
fn replace_or_union(source: Option<&String>, target: &mut Option<String>, replace: bool) {
    if replace || list(target.as_deref()).is_empty() {
        target.clone_from(&source.cloned());
    } else {
        *target = union(target.as_deref(), source.map(String::as_str));
    }
}

/// Whether upstream's item class for `kind` derives from `Audio`, `Video` or
/// `Book` — the classes whose runtime comes from their own media, never
/// from a provider (`MergeBaseItemData` `:1260-1269`).
fn owns_its_runtime(kind: Option<BaseItemKind>) -> bool {
    matches!(
        kind,
        Some(
            BaseItemKind::Audio
                | BaseItemKind::AudioBook
                | BaseItemKind::Book
                | BaseItemKind::Movie
                | BaseItemKind::Episode
                | BaseItemKind::Trailer
                | BaseItemKind::MusicVideo
                | BaseItemKind::Video
        )
    )
}

/// `IHasAlbumArtist` (`Audio` and its `AudioBook`, `MusicAlbum`,
/// `MusicVideo`).
fn has_album_artist(kind: Option<BaseItemKind>) -> bool {
    matches!(
        kind,
        Some(
            BaseItemKind::Audio
                | BaseItemKind::AudioBook
                | BaseItemKind::MusicAlbum
                | BaseItemKind::MusicVideo
        )
    )
}

/// `MergeBaseItemData` over the row's columns and its `Data` blob.
fn merge_base_item(
    source: &BaseItemEntity,
    target: &mut BaseItemEntity,
    unlocked: &dyn Fn(MetadataField) -> bool,
    replace: bool,
    merge_metadata_settings: bool,
) {
    // "Safeguard against incoming data having an empty name."
    if unlocked(MetadataField::Name)
        && (replace || is_empty(target.name.as_deref()))
        && !is_blank(source.name.as_deref())
    {
        target.name.clone_from(&source.name);
    }
    text(
        source.original_title.as_ref(),
        &mut target.original_title,
        replace,
    );
    text(
        source.original_language.as_ref(),
        &mut target.original_language,
        replace,
    );
    value(
        source.community_rating.as_ref(),
        &mut target.community_rating,
        replace,
    );
    value(source.end_date.as_ref(), &mut target.end_date, replace);
    if unlocked(MetadataField::Genres) && (replace || list(target.genres.as_deref()).is_empty()) {
        target.genres.clone_from(&source.genres);
    }
    value(
        source.index_number.as_ref(),
        &mut target.index_number,
        replace,
    );
    if unlocked(MetadataField::OfficialRating) {
        text(
            source.official_rating.as_ref(),
            &mut target.official_rating,
            replace,
        );
    }
    text(
        source.custom_rating.as_ref(),
        &mut target.custom_rating,
        replace,
    );
    text(source.tagline.as_ref(), &mut target.tagline, replace);
    if unlocked(MetadataField::Overview) {
        text(source.overview.as_ref(), &mut target.overview, replace);
    }
    value(
        source.parent_index_number.as_ref(),
        &mut target.parent_index_number,
        replace,
    );
    value(
        source.premiere_date.as_ref(),
        &mut target.premiere_date,
        replace,
    );
    value(
        source.production_year.as_ref(),
        &mut target.production_year,
        replace,
    );
    if unlocked(MetadataField::Runtime)
        && (replace || target.run_time_ticks.is_none())
        && !owns_its_runtime(BaseItemKind::from_stored_type_name(&target.type_))
    {
        target.run_time_ticks = source.run_time_ticks;
    }
    merge_union_lists(source, target, unlocked, replace);
    value(
        source.critic_rating.as_ref(),
        &mut target.critic_rating,
        replace,
    );
    if (replace || is_empty(target.forced_sort_name.as_deref()))
        && !is_empty(source.forced_sort_name.as_deref())
    {
        target.forced_sort_name.clone_from(&source.forced_sort_name);
    }
    // `RemoteTrailers`, `Video3DFormat` (`MergeVideoInfo`), `DisplayOrder`
    // (`MergeDisplayOrder`) and the per-kind blob properties live in `Data`.
    target.data = merge_data_blob(source.data.as_deref(), target.data.as_deref(), replace);
    if merge_metadata_settings {
        merge_settings(source, target, replace);
    }
}

/// The `mergeMetadataSettings` half of `MergeBaseItemData` (`:1365-1402`).
fn merge_settings(source: &BaseItemEntity, target: &mut BaseItemEntity, replace: bool) {
    if replace || !target.is_locked {
        target.is_locked = target.is_locked || source.is_locked;
    }
    // `DateTime.MinValue` is upstream's "not set"; here it is `None`.
    if source.date_created.is_some() {
        target.date_created = source.date_created;
    }
    if replace || source.date_modified.is_some() {
        target.date_modified = source.date_modified;
    }
    text(
        source.preferred_metadata_country_code.as_ref(),
        &mut target.preferred_metadata_country_code,
        replace,
    );
    text(
        source.preferred_metadata_language.as_ref(),
        &mut target.preferred_metadata_language,
        replace,
    );
}

/// The list fields that union on a fill merge — `Studios`, `Tags`,
/// `ProductionLocations` (`:1271-1305`) and `MergeAlbumArtist`
/// (`:1488-1502`).
fn merge_union_lists(
    source: &BaseItemEntity,
    target: &mut BaseItemEntity,
    unlocked: &dyn Fn(MetadataField) -> bool,
    replace: bool,
) {
    if unlocked(MetadataField::Studios) {
        replace_or_union(source.studios.as_ref(), &mut target.studios, replace);
    }
    if unlocked(MetadataField::Tags) {
        replace_or_union(source.tags.as_ref(), &mut target.tags, replace);
    }
    if unlocked(MetadataField::ProductionLocations) {
        replace_or_union(
            source.production_locations.as_ref(),
            &mut target.production_locations,
            replace,
        );
    }
    // `MergeAlbumArtist` (`:1488-1502`).
    let kind = BaseItemKind::from_stored_type_name(&target.type_);
    if has_album_artist(kind)
        && has_album_artist(BaseItemKind::from_stored_type_name(&source.type_))
    {
        if replace || list(target.album_artists.as_deref()).is_empty() {
            target.album_artists.clone_from(&source.album_artists);
        } else if !list(source.album_artists.as_deref()).is_empty() {
            target.album_artists = union(
                target.album_artists.as_deref(),
                source.album_artists.as_deref(),
            );
        }
    }
}

/// The per-kind `MergeData` overrides that touch columns: `Artists` and
/// `Album` (`AudioMetadataService`, `AlbumMetadataService`,
/// `MusicVideoMetadataService`, `AudioBookMetadataService`), a book's
/// `SeriesName` (`BookMetadataService`) and an episode's season number
/// (`EpisodeMetadataService`). Their blob-held properties are in
/// [`DATA_RULES`].
fn merge_kind_specific(
    source: &BaseItemEntity,
    target: &mut BaseItemEntity,
    replace: bool,
    merge_metadata_settings: bool,
) {
    match BaseItemKind::from_stored_type_name(&target.type_) {
        Some(BaseItemKind::Audio | BaseItemKind::MusicAlbum | BaseItemKind::MusicVideo) => {
            if replace || list(target.artists.as_deref()).is_empty() {
                target.artists.clone_from(&source.artists);
            } else {
                target.artists = union(target.artists.as_deref(), source.artists.as_deref());
            }
            if !matches!(
                BaseItemKind::from_stored_type_name(&target.type_),
                Some(BaseItemKind::MusicAlbum)
            ) {
                text(source.album.as_ref(), &mut target.album, replace);
            }
        }
        Some(BaseItemKind::AudioBook) => {
            if replace || list(target.artists.as_deref()).is_empty() {
                target.artists.clone_from(&source.artists);
            }
            text(source.album.as_ref(), &mut target.album, replace);
        }
        Some(BaseItemKind::Book) => {
            text(
                source.series_name.as_ref(),
                &mut target.series_name,
                replace,
            );
        }
        // "Episode season numbers can be set from path parsing before local
        // metadata is merged. When a provider supplies an explicit season,
        // prefer it" — only on the merges that carry settings.
        Some(BaseItemKind::Episode)
            if merge_metadata_settings && source.parent_index_number.is_some() =>
        {
            target.parent_index_number = source.parent_index_number;
        }
        _ => {}
    }
}

/// How one `Data`-blob property merges.
#[derive(Clone, Copy)]
enum DataRule {
    /// `if (replaceData || target empty) target.X = source.X` — replace may
    /// clear it.
    Plain,
    /// `RemoteTrailers`: replace, or union by `Url`.
    Trailers,
    /// `DisplayOrder`: only a non-blank source value is taken.
    NonBlankSource,
    /// `Video3DFormat`: only a source that has a value is taken.
    PresentSource,
}

/// The upstream properties Ferrofin keeps in `Data`, with their merge rule.
/// Every other key is file- or custom-provider-derived and merges key-wise
/// (see [`merge_data_blob`]).
const DATA_RULES: &[(&str, DataRule)] = &[
    ("RemoteTrailers", DataRule::Trailers),
    ("DisplayOrder", DataRule::NonBlankSource),
    ("Video3DFormat", DataRule::PresentSource),
    // `SeriesMetadataService.MergeData`.
    ("AirTime", DataRule::Plain),
    ("Status", DataRule::Plain),
    ("AirDays", DataRule::Plain),
    // `EpisodeMetadataService.MergeData`.
    ("AirsBeforeSeasonNumber", DataRule::Plain),
    ("AirsAfterSeasonNumber", DataRule::Plain),
    ("AirsBeforeEpisodeNumber", DataRule::Plain),
    ("IndexNumberEnd", DataRule::Plain),
    // `MovieMetadataService.MergeData`.
    ("CollectionName", DataRule::Plain),
    // `TrailerMetadataService.MergeData`.
    ("TrailerTypes", DataRule::Plain),
];

/// Whether a blob value is "empty" for the fill rules: absent, `null`, `""`
/// or `[]`.
fn blob_empty(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        Some(Value::Array(a)) => a.is_empty(),
        Some(_) => false,
    }
}

/// The union of two `RemoteTrailers` arrays, deduplicated by `Url`
/// (`DistinctBy(t => t.Url)`, first occurrence kept).
fn union_trailers(target: &Value, source: Option<&Value>) -> Value {
    let mut seen = HashSet::new();
    let entries = target
        .as_array()
        .into_iter()
        .flatten()
        .chain(source.and_then(Value::as_array).into_iter().flatten())
        .filter(|entry| seen.insert(entry.get("Url").and_then(Value::as_str).map(str::to_owned)))
        .cloned()
        .collect();
    Value::Array(entries)
}

/// Merges two `Data` column values key by key; returns `target` unchanged
/// (byte for byte) when no key changes, so an unchanged row is not
/// reserialized.
fn merge_data_blob(source: Option<&str>, target: Option<&str>, replace: bool) -> Option<String> {
    let src = parse_data(source);
    let mut out = parse_data(target);
    let before = out.clone();
    for (key, rule) in DATA_RULES {
        let incoming = src.get(*key);
        let current = out.get(*key);
        let next: Option<Value> = match rule {
            DataRule::Plain => {
                if replace || blob_empty(current) {
                    incoming.cloned()
                } else {
                    continue;
                }
            }
            DataRule::Trailers => {
                if replace || blob_empty(current) {
                    incoming.cloned()
                } else {
                    Some(union_trailers(current.unwrap_or(&Value::Null), incoming))
                }
            }
            DataRule::NonBlankSource => {
                let take = (replace || blob_empty(current))
                    && incoming
                        .and_then(Value::as_str)
                        .is_some_and(|v| !v.trim().is_empty());
                if !take {
                    continue;
                }
                incoming.cloned()
            }
            DataRule::PresentSource => {
                if blob_empty(incoming) || !(replace || blob_empty(current)) {
                    continue;
                }
                incoming.cloned()
            }
        };
        match next {
            Some(v) => {
                out.insert((*key).to_owned(), v);
            }
            None => {
                out.remove(*key);
            }
        }
    }
    // Every other key (`VideoType`, EXIF fields, …) is set on the item by the
    // resolver or a custom provider, not by `MergeData`: the source's value is
    // taken when it has one (fill-only when not replacing), and a key only
    // the target holds is kept.
    for (key, v) in &src {
        if DATA_RULES.iter().any(|(k, _)| k == key) {
            continue;
        }
        if replace || blob_empty(out.get(key)) {
            out.insert(key.clone(), v.clone());
        }
    }
    if out == before {
        return target.map(str::to_owned);
    }
    if out.is_empty() && target.is_none() {
        return None;
    }
    serde_json::to_string(&Value::Object(out))
        .ok()
        .or_else(|| target.map(str::to_owned))
}

/// The people part of `MergeBaseItemData` (`:1235-1249`): replace on
/// `replaceData` or an empty target, else enrich the target's people from
/// the source's (`MergePeople`) without adding anyone.
fn merge_people_field(
    source: Option<&[PeopleEntity]>,
    target: &mut Option<Vec<PeopleEntity>>,
    replace: bool,
) {
    let source: Option<Vec<PeopleEntity>> = source.map(remove_invalid_provider_ids);
    if let Some(people) = target.as_mut() {
        *people = remove_invalid_provider_ids(people);
    }
    if replace || target.as_ref().is_none_or(Vec::is_empty) {
        *target = source;
    } else if let (Some(source), Some(target)) = (source, target.as_mut())
        && !source.is_empty()
    {
        merge_people(&source, target);
    }
}

/// `RemoveInvalidProviderIds` over Ferrofin's one person id (the TMDB id).
fn remove_invalid_provider_ids(people: &[PeopleEntity]) -> Vec<PeopleEntity> {
    people
        .iter()
        .cloned()
        .map(|mut p| {
            if p.provider_id.is_some_and(|id| id <= 0) {
                p.provider_id = None;
            }
            p
        })
        .collect()
}

/// The name key `MergePeople` groups by: diacritics removed, compared
/// ordinal-ignore-case.
fn person_key(name: &str) -> String {
    ferrofin_util::string_extensions::upper_invariant(
        &ferrofin_util::string_extensions::remove_diacritics(name),
    )
}

/// `MergePeople` (`:1429-1470`): for each target person, the same-named
/// source person at the same position (or the first one) fills the
/// target's missing provider id, image and role. Nobody is added.
pub fn merge_people(source: &[PeopleEntity], target: &mut [PeopleEntity]) {
    let mut positions: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for person in target.iter_mut() {
        let key = person_key(&person.name);
        let same_named: Vec<&PeopleEntity> = source
            .iter()
            .filter(|p| person_key(&p.name) == key)
            .collect();
        let index = positions.entry(key).or_insert(0);
        let i = *index;
        *index += 1;
        let Some(first) = same_named.first() else {
            continue;
        };
        let in_source = same_named.get(i).unwrap_or(first);
        if person.provider_id.is_none() {
            person.provider_id = in_source.provider_id;
        }
        if is_blank(person.primary_image_url.as_deref()) {
            person
                .primary_image_url
                .clone_from(&in_source.primary_image_url);
        }
        if !is_blank(in_source.role.as_deref()) && is_blank(person.role.as_deref()) {
            person.role.clone_from(&in_source.role);
        }
    }
}

/// The ProviderIds rule of `MergeBaseItemData` (`:1307-1334`): a source id
/// that is valid for its provider replaces the target's on `replaceData`, or
/// when the target has none or an unusable one; then every invalid id left
/// on the target is dropped. Keys compare ordinal-ignore-case, as the
/// upstream dictionary does.
#[must_use]
pub fn merge_provider_ids(
    source: &[(String, String)],
    target: &[(String, String)],
    replace: bool,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = target.to_vec();
    for (key, value) in source {
        if !is_valid_provider_id(key, value) {
            continue;
        }
        match out.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(key)) {
            Some(existing) => {
                if replace || !is_valid_provider_id(&existing.0, &existing.1) {
                    existing.1.clone_from(value);
                }
            }
            None => out.push((key.clone(), value.clone())),
        }
    }
    out.retain(|(k, v)| is_valid_provider_id(k, v));
    out
}

/// `ProviderIdsExtensions.IsValidProviderId`: a blank name or value is never
/// valid; a provider with a known id shape must match it (IMDb ids, positive
/// TMDB/AudioDb numbers, MusicBrainz MBIDs); any other provider accepts any
/// value.
#[must_use]
pub fn is_valid_provider_id(name: &str, value: &str) -> bool {
    if name.trim().is_empty() || value.trim().is_empty() {
        return false;
    }
    let is = |known: &str| name.eq_ignore_ascii_case(known);
    if is("Imdb") {
        is_imdb_id(value)
    } else if is("Tmdb") || is("TmdbCollection") || is("AudioDbArtist") || is("AudioDbAlbum") {
        is_positive_number(value)
    } else if is("MusicBrainzAlbum")
        || is("MusicBrainzAlbumArtist")
        || is("MusicBrainzArtist")
        || is("MusicBrainzReleaseGroup")
        || is("MusicBrainzRecording")
        || is("MusicBrainzTrack")
    {
        uuid::Uuid::parse_str(value).is_ok()
    } else {
        true
    }
}

/// `int.TryParse(value, NumberStyles.None, …) && id > 0`: digits only, no
/// sign or whitespace, fitting an `int`.
fn is_positive_number(value: &str) -> bool {
    value.bytes().all(|b| b.is_ascii_digit()) && value.parse::<i32>().is_ok_and(|id| id > 0)
}

/// `^(tt|nm|co|ev|ch|ni)?[0-9]+$`, case-insensitive.
fn is_imdb_id(value: &str) -> bool {
    let digits = match value.get(..2) {
        Some(prefix)
            if ["tt", "nm", "co", "ev", "ch", "ni"]
                .iter()
                .any(|p| prefix.eq_ignore_ascii_case(p)) =>
        {
            &value[2..]
        }
        _ => value,
    };
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests;
