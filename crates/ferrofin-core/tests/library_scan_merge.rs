//! What a rescan does to an item that is already stored: it saves the stored
//! row with this pass's file facts and provider results merged on
//! (upstream's Default refresh, `MetadataService.RefreshWithProviders`),
//! never the row the planner rebuilt from disk.
//!
//! Before that merge, every scan wrote the disk-rebuilt row, so anything a
//! provider had supplied was NULLed whenever the provider did not answer
//! again, and the refresh dates were wiped on every pass.

use std::io::{Read as _, Write as _};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::{ItemTypeLookup, derive_item_id};
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_providers::TmdbClient;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::persistence::{ItemPersistenceService as _, ItemRepository};

/// `/search/movie`.
const SEARCH_JSON: &str = r#"{"results": [{"id": 603, "title": "The Matrix"}]}"#;

/// `/movie/603` with its appended credits and videos.
fn details_json(overview: &str) -> String {
    format!(
        r#"{{
        "title": "The Matrix",
        "overview": "{overview}",
        "tagline": "Welcome to the Real World.",
        "genres": [{{"name": "Action"}}, {{"name": "Science Fiction"}}],
        "production_companies": [{{"name": "Village Roadshow"}}],
        "vote_average": 8.2,
        "release_date": "1999-03-30",
        "videos": {{"results": [
            {{"site": "YouTube", "type": "Trailer", "key": "vKQi3bBA1y8", "name": "Trailer"}}
        ]}}
    }}"#
    )
}

/// A TMDB stand-in answering the movie search and details, counting the
/// details requests.
fn spawn_tmdb(overview: &'static str) -> (String, Arc<AtomicUsize>) {
    let details = Arc::new(AtomicUsize::new(0));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let counter = Arc::clone(&details);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let line = req.lines().next().unwrap_or_default().to_owned();
            let (status, payload) = if line.contains("/search/movie") {
                ("200 OK", SEARCH_JSON.to_owned())
            } else if line.contains("/movie/603?") {
                counter.fetch_add(1, Ordering::SeqCst);
                ("200 OK", details_json(overview))
            } else {
                ("404 Not Found", "{}".to_owned())
            };
            let _ = write!(
                s,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
        }
    });
    (format!("http://{addr}"), details)
}

/// One library of `kind` over `media`, on a fresh in-memory database.
struct Fixture {
    db: Database,
    vf: Arc<dyn VirtualFolderManager>,
    persistence: Arc<FerrofinItemPersistenceService>,
    items: Arc<dyn ItemRepository>,
}

impl Fixture {
    async fn new(root: &Path, media: &Path, kind: CollectionTypeOptions) -> Self {
        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrate");
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(root.join("views"))
                .with_item_store(persistence.clone()),
        );
        vf.add_virtual_folder(
            "Library",
            Some(kind),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: media.to_string_lossy().into_owned(),
                }],
                ..LibraryOptions::default()
            },
        )
        .await
        .expect("add library");
        let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        ));
        Self {
            db,
            vf,
            persistence,
            items,
        }
    }

    /// A scanner with no remote provider.
    fn scanner(&self) -> LibraryScanner {
        LibraryScanner::new(
            Arc::clone(&self.vf),
            Arc::new(FerrofinFileSystem::new()),
            self.persistence.clone(),
        )
        .with_items(Arc::clone(&self.items))
    }

    /// A scanner fetching from the TMDB stand-in at `base`.
    fn tmdb_scanner(&self, base: &str, root: &Path) -> LibraryScanner {
        self.scanner().with_metadata(
            Arc::new(TmdbClient::new().with_base_url(base)),
            root.join("metadata"),
        )
    }

    async fn row(&self, kind: BaseItemKind, path: &Path) -> BaseItemEntity {
        let id = derive_item_id(kind, &path.to_string_lossy()).expect("id");
        self.items
            .retrieve_item(id)
            .await
            .expect("read")
            .expect("row")
    }

    /// The raw stored `DateLastSaved` / `DateModified` text of a row.
    async fn raw_dates(&self, kind: BaseItemKind, path: &Path) -> (Option<String>, Option<String>) {
        let id = derive_item_id(kind, &path.to_string_lossy()).expect("id");
        sqlx::query_as(r#"SELECT "DateLastSaved", "DateModified" FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(ferrofin_db::store::guid_to_db(id))
            .fetch_one(self.db.pool())
            .await
            .expect("row")
    }
}

fn trailer_urls(row: &BaseItemEntity) -> Vec<String> {
    ferrofin_core::item_data::read_remote_trailers(row.data.as_deref())
        .into_iter()
        .map(|(_, url)| url)
        .collect()
}

/// A no-NFO movie keeps everything TMDB supplied when a later scan runs
/// without TMDB (an outage, an unticked fetcher, or — from Phase 4 — a scan
/// that has no reason to ask again). When TMDB does answer again, its values
/// win: the Default refresh replaces, with the stored row as the fallback.
#[tokio::test]
async fn a_rescan_keeps_what_the_providers_supplied() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    let folder = media.join("The Matrix (1999)");
    std::fs::create_dir_all(&folder).expect("mkdir");
    let file = folder.join("The Matrix (1999).mkv");
    std::fs::write(&file, b"0123456789").expect("write");
    let fx = Fixture::new(tmp.path(), &media, CollectionTypeOptions::movies).await;

    let (base, details) = spawn_tmdb("A hacker learns the truth.");
    fx.tmdb_scanner(&base, tmp.path())
        .scan_all()
        .await
        .expect("first scan");
    assert_eq!(details.load(Ordering::SeqCst), 1);
    let first = fx.row(BaseItemKind::Movie, &file).await;
    assert_eq!(
        first.overview.as_deref(),
        Some("A hacker learns the truth.")
    );
    assert_eq!(first.size, Some(10), "Size is the file's length");
    assert_eq!(
        fx.raw_dates(BaseItemKind::Movie, &file).await.0,
        None,
        "a created row has no DateLastSaved"
    );

    // No provider this time.
    fx.scanner().scan_all().await.expect("rescan");
    let kept = fx.row(BaseItemKind::Movie, &file).await;
    assert_eq!(kept.overview, first.overview);
    assert_eq!(kept.tagline.as_deref(), Some("Welcome to the Real World."));
    assert_eq!(kept.genres.as_deref(), Some("Action|Science Fiction"));
    assert_eq!(kept.studios.as_deref(), Some("Village Roadshow"));
    assert_eq!(kept.community_rating, Some(8.2));
    assert_eq!(kept.premiere_date, first.premiere_date);
    assert!(kept.premiere_date.is_some());
    assert_eq!(kept.production_year, Some(1999));
    assert_eq!(
        trailer_urls(&kept),
        ["https://www.youtube.com/watch?v=vKQi3bBA1y8"]
    );
    assert!(
        fx.raw_dates(BaseItemKind::Movie, &file).await.0.is_some(),
        "an updated row is stamped with its save time"
    );

    // TMDB answers again with a new synopsis: the provider wins.
    let (base, _) = spawn_tmdb("Reality is a simulation.");
    fx.tmdb_scanner(&base, tmp.path())
        .scan_all()
        .await
        .expect("third scan");
    let replaced = fx.row(BaseItemKind::Movie, &file).await;
    assert_eq!(
        replaced.overview.as_deref(),
        Some("Reality is a simulation.")
    );
    assert_eq!(trailer_urls(&replaced).len(), 1, "trailers union by URL");
}

/// A locked row is the user's: a rescan runs no provider for it and leaves
/// its metadata and its whole `Data` blob (the trailers) as they were.
#[tokio::test]
async fn a_locked_rows_metadata_and_data_survive_a_rescan() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    let folder = media.join("The Matrix (1999)");
    std::fs::create_dir_all(&folder).expect("mkdir");
    let file = folder.join("The Matrix (1999).mkv");
    std::fs::write(&file, b"").expect("write");
    let fx = Fixture::new(tmp.path(), &media, CollectionTypeOptions::movies).await;
    let (base, details) = spawn_tmdb("A hacker learns the truth.");
    let scanner = fx.tmdb_scanner(&base, tmp.path());
    scanner.scan_all().await.expect("first scan");
    let before = fx.row(BaseItemKind::Movie, &file).await;
    sqlx::query(r#"UPDATE "BaseItems" SET "IsLocked" = 1 WHERE "Type" LIKE '%Movies.Movie'"#)
        .execute(fx.db.writer())
        .await
        .expect("lock");

    scanner.scan_all().await.expect("rescan");
    assert_eq!(
        details.load(Ordering::SeqCst),
        1,
        "no provider runs for a locked row"
    );
    let after = fx.row(BaseItemKind::Movie, &file).await;
    assert!(after.is_locked);
    assert_eq!(after.data, before.data, "the Data blob is untouched");
    assert_eq!(after.overview, before.overview);
    assert_eq!(after.genres, before.genres);
}

/// Owner decision D3: a folder's `DateModified` is its directory's mtime,
/// stamped on every save, as upstream's `SaveInternal` does.
#[tokio::test]
async fn a_folders_date_modified_is_its_directory_mtime() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("tv");
    let series = media.join("Show");
    let season = series.join("Season 01");
    std::fs::create_dir_all(&season).expect("mkdir");
    std::fs::write(season.join("Show S01E01.mkv"), b"").expect("write");
    let past = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    for dir in [&series, &season] {
        std::fs::File::open(dir)
            .expect("open dir")
            .set_modified(past)
            .expect("set mtime");
    }
    let fx = Fixture::new(tmp.path(), &media, CollectionTypeOptions::tvshows).await;
    fx.scanner().scan_all().await.expect("scan");
    let expected = Some(ferrofin_db::store::datetime_to_db(past.into()));
    assert_eq!(
        fx.raw_dates(BaseItemKind::Series, &series).await.1,
        expected
    );
    assert_eq!(
        fx.raw_dates(BaseItemKind::Season, &season).await.1,
        expected
    );

    // A later change to the directory is picked up by the next save.
    let later = past + std::time::Duration::from_hours(24);
    std::fs::File::open(&series)
        .expect("open dir")
        .set_modified(later)
        .expect("set mtime");
    fx.scanner().scan_all().await.expect("rescan");
    assert_eq!(
        fx.raw_dates(BaseItemKind::Series, &series).await.1,
        Some(ferrofin_db::store::datetime_to_db(later.into()))
    );
}

/// An album's release date (MusicBrainz's `apply_release_details` writes it
/// after the scan) survives the next scan. The disk-rebuilt row carried none,
/// so every scan NULLed it and the music pass refetched every album.
#[tokio::test]
async fn an_albums_premiere_date_survives_a_rescan() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("music");
    let album = media.join("Artist").join("Album");
    std::fs::create_dir_all(&album).expect("mkdir");
    std::fs::write(album.join("01 - Song.mp3"), b"").expect("write");
    let fx = Fixture::new(tmp.path(), &media, CollectionTypeOptions::music).await;
    let scanner = fx.scanner();
    scanner.scan_all().await.expect("first scan");

    // What the music pass writes for a matched release.
    let mut row = fx.row(BaseItemKind::MusicAlbum, &album).await;
    let released = chrono::DateTime::parse_from_rfc3339("1997-05-21T00:00:00Z")
        .expect("date")
        .with_timezone(&chrono::Utc);
    row.premiere_date = Some(released);
    row.production_year = Some(1997);
    fx.persistence
        .save_items(std::slice::from_ref(&row))
        .await
        .expect("enrich");

    scanner.scan_all().await.expect("rescan");
    let kept = fx.row(BaseItemKind::MusicAlbum, &album).await;
    assert_eq!(kept.premiere_date, Some(released));
    assert_eq!(kept.production_year, Some(1997));
}

/// The ProviderIds rule: an id that cannot belong to its provider (a stored
/// `Tmdb` that is not a positive number) is dropped by the next save, while
/// the valid ones stay.
#[tokio::test]
async fn a_rescan_drops_a_stored_provider_id_of_the_wrong_shape() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    let folder = media.join("The Matrix (1999)");
    std::fs::create_dir_all(&folder).expect("mkdir");
    let file = folder.join("The Matrix (1999).mkv");
    std::fs::write(&file, b"").expect("write");
    let fx = Fixture::new(tmp.path(), &media, CollectionTypeOptions::movies).await;
    let scanner = fx.scanner();
    scanner.scan_all().await.expect("first scan");
    let id = derive_item_id(BaseItemKind::Movie, &file.to_string_lossy()).expect("id");
    fx.persistence
        .save_provider_id(id, "Tmdb", "nm0000123")
        .await
        .expect("bad id");
    fx.persistence
        .save_provider_id(id, "Imdb", "tt0133093")
        .await
        .expect("good id");

    scanner.scan_all().await.expect("rescan");
    let ids = fx
        .persistence
        .provider_ids_for_items(&[id])
        .await
        .expect("ids")
        .remove(&id)
        .unwrap_or_default();
    assert_eq!(ids, [("Imdb".to_owned(), "tt0133093".to_owned())]);
}
