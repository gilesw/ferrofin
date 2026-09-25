//! Per-item change detection, end to end through a real scan
//! (`PLAN_SCAN_CHANGE_DETECTION` Phase 4): the port of
//! `MetadataService.RefreshMetadata`'s provider selection and save rule.
//!
//! Over the real scanner, repositories and SQLite schema, with an ffprobe
//! stand-in that records every probed path, a TMDB stand-in that records every
//! request, and a trigger on every table that counts every row written:
//!
//! - an unchanged rescan probes nothing, asks no provider and writes nothing;
//! - a file whose mtime moved is probed, fetched and saved — only that one;
//! - an NFO newer than the last save re-reads only that item's local metadata;
//! - a sidecar subtitle added beside a video re-probes only that video;
//! - a never-refreshed item runs everything once, then goes quiet;
//! - an elapsed `AutomaticRefreshIntervalDays` refetches;
//! - a provider that fails leaves the item unstamped (retried next scan), one
//!   that finds nothing stamps it (owner decision D1).

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::{ItemTypeLookup, derive_item_id};
use ferrofin_core::{
    FerrofinChapterRepository, FerrofinItemPersistenceService, FerrofinItemRepository,
    FerrofinMediaStreamRepository, FerrofinVirtualFolderManager, LibraryScanner, ScanOutcome,
};
use ferrofin_db::Database;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::{CollectionTypeOptions, MediaStreamType, Video3DFormat};
use ferrofin_model::entities_media::MediaStream;
use ferrofin_providers::TmdbClient;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::media_encoding::{MediaEncoder, MediaInfoRequest};
use ferrofin_traits::persistence::{ItemRepository, MediaStreamRepository};

/// An ffprobe stand-in recording each probed path: a video file has a video
/// and an audio stream, a `.srt` one subtitle stream.
#[derive(Default)]
struct RecordingProbe {
    probed: Mutex<Vec<String>>,
}

impl RecordingProbe {
    /// The file names probed since the last call.
    fn take(&self) -> Vec<String> {
        let mut probed = self.probed.lock().expect("lock");
        let mut names: Vec<String> = probed
            .drain(..)
            .map(|p| {
                Path::new(&p)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        names.sort_unstable();
        names
    }
}

#[async_trait]
impl MediaEncoder for RecordingProbe {
    fn encoder_path(&self) -> String {
        "ffmpeg".to_owned()
    }
    fn probe_path(&self) -> String {
        "ffprobe".to_owned()
    }
    async fn set_ffmpeg_path(&self) -> Result<bool, ServiceError> {
        Ok(true)
    }
    async fn get_media_info(
        &self,
        request: &MediaInfoRequest,
    ) -> Result<MediaSourceInfo, ServiceError> {
        let path = request.media_source.path.clone().unwrap_or_default();
        self.probed.lock().expect("lock").push(path.clone());
        let streams = if Path::new(&path)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("srt"))
        {
            vec![MediaStream {
                index: 0,
                stream_type: MediaStreamType::Subtitle,
                codec: Some("subrip".to_owned()),
                ..MediaStream::default()
            }]
        } else {
            vec![
                MediaStream {
                    index: 0,
                    stream_type: MediaStreamType::Video,
                    codec: Some("h264".to_owned()),
                    width: Some(1920),
                    height: Some(1080),
                    ..MediaStream::default()
                },
                MediaStream {
                    index: 1,
                    stream_type: MediaStreamType::Audio,
                    codec: Some("aac".to_owned()),
                    ..MediaStream::default()
                },
            ]
        };
        Ok(MediaSourceInfo {
            run_time_ticks: Some(72_000_000_000),
            bitrate: Some(8_000_000),
            media_streams: streams,
            ..MediaSourceInfo::default()
        })
    }
    async fn extract_audio_image(
        &self,
        _path: &str,
        _image_stream_index: Option<i32>,
    ) -> Result<String, ServiceError> {
        unreachable!("no audio in this library")
    }
    async fn extract_video_image(
        &self,
        _input_file: &str,
        _container: &str,
        _media_source: &MediaSourceInfo,
        _video_stream: &MediaStream,
        _threed_format: Option<Video3DFormat>,
        _offset_ticks: Option<i64>,
    ) -> Result<String, ServiceError> {
        unreachable!("no frame extraction in a scan")
    }
    fn get_input_argument(&self, input_file: &str, _media_source: &MediaSourceInfo) -> String {
        input_file.to_owned()
    }
    fn get_time_parameter(&self, _ticks: i64) -> String {
        String::new()
    }
    async fn convert_image(&self, _i: &str, _o: &str) -> Result<(), ServiceError> {
        Ok(())
    }
}

/// `/movie/{id}` details, trailers included, so no backfill heuristic (D2)
/// ever asks again on its own.
fn details_json(title: &str) -> String {
    format!(
        r#"{{"title": "{title}", "overview": "About {title}.", "vote_average": 8.0,
            "release_date": "1999-03-30",
            "videos": {{"results": [
                {{"site": "YouTube", "type": "Trailer", "key": "k{title}", "name": "Trailer"}}
            ]}}}}"#
    )
}

/// How the TMDB stand-in answers a movie's details.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// A full record.
    Found,
    /// The search finds nothing: an answer, not a failure.
    Nothing,
    /// `401` on every request for it: a failure.
    Fail,
}

/// A TMDB stand-in for two movies (The Matrix → 603, Heat → 949), recording
/// each request line.
struct Tmdb {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
    heat: Arc<Mutex<Answer>>,
}

impl Tmdb {
    fn spawn() -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let heat = Arc::new(Mutex::new(Answer::Found));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (log, answer) = (Arc::clone(&requests), Arc::clone(&heat));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let line = req.lines().next().unwrap_or_default().to_owned();
                log.lock().expect("lock").push(line.clone());
                let heat = *answer.lock().expect("lock");
                let is_heat = line.contains("Heat") || line.contains("/movie/949");
                let (status, payload) = if is_heat && heat == Answer::Fail {
                    ("401 Unauthorized", "{}".to_owned())
                } else if line.contains("/search/movie") {
                    let hit = if is_heat {
                        if heat == Answer::Nothing {
                            r#"{"results": []}"#
                        } else {
                            r#"{"results": [{"id": 949, "title": "Heat"}]}"#
                        }
                    } else {
                        r#"{"results": [{"id": 603, "title": "The Matrix"}]}"#
                    };
                    ("200 OK", hit.to_owned())
                } else if line.contains("/movie/603?") {
                    ("200 OK", details_json("The Matrix"))
                } else if line.contains("/movie/949?") {
                    ("200 OK", details_json("Heat"))
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
        Self {
            base: format!("http://{addr}"),
            requests,
            heat,
        }
    }

    /// The request lines since the last call.
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.requests.lock().expect("lock"))
    }

    fn answer_heat(&self, answer: Answer) {
        *self.heat.lock().expect("lock") = answer;
    }
}

/// A movie library of two titles, each in its own folder, scanned by a
/// scanner with the probe, TMDB and the item repository wired.
struct Fixture {
    db: Database,
    scanner: LibraryScanner,
    probe: Arc<RecordingProbe>,
    tmdb: Tmdb,
    matrix: PathBuf,
    heat: PathBuf,
}

impl Fixture {
    async fn new(root: &Path, interval_days: i32) -> Self {
        let media = root.join("movies");
        let matrix = media
            .join("The Matrix (1999)")
            .join("The Matrix (1999).mkv");
        let heat = media.join("Heat (1995)").join("Heat (1995).mkv");
        for file in [&matrix, &heat] {
            std::fs::create_dir_all(file.parent().expect("dir")).expect("mkdir");
            std::fs::write(file, b"0123456789").expect("write");
        }
        let db = Database::connect_in_memory().await.expect("connect");
        db.run_migrations().await.expect("migrate");
        let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(root.join("views"))
                .with_item_store(persistence.clone()),
        );
        vf.add_virtual_folder(
            "Movies",
            Some(CollectionTypeOptions::movies),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: media.to_string_lossy().into_owned(),
                }],
                automatic_refresh_interval_days: interval_days,
                ..LibraryOptions::default()
            },
        )
        .await
        .expect("add library");
        let items: Arc<dyn ItemRepository> = Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        ));
        let probe = Arc::new(RecordingProbe::default());
        let tmdb = Tmdb::spawn();
        let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
            .with_items(items)
            .with_probe(
                Arc::clone(&probe) as Arc<dyn MediaEncoder>,
                Arc::new(FerrofinMediaStreamRepository::new(db.clone()))
                    as Arc<dyn MediaStreamRepository>,
                Arc::new(FerrofinChapterRepository::new(db.clone())),
            )
            .with_metadata(
                Arc::new(TmdbClient::new().with_base_url(&tmdb.base)),
                root.join("metadata"),
            )
            .with_progress_every(0);
        count_writes(&db).await;
        Self {
            db,
            scanner,
            probe,
            tmdb,
            matrix,
            heat,
        }
    }

    async fn scan(&self) -> ScanOutcome {
        self.scanner.scan_all().await.expect("scan")
    }

    fn id(path: &Path) -> String {
        guid_to_db(derive_item_id(BaseItemKind::Movie, &path.to_string_lossy()).expect("id"))
    }

    /// `(DateLastSaved, DateLastRefreshed)` as stored, per movie path.
    async fn stamps(&self, path: &Path) -> (Option<String>, Option<String>) {
        sqlx::query_as(
            r#"SELECT "DateLastSaved", "DateLastRefreshed" FROM "BaseItems" WHERE "Id" = ?1"#,
        )
        .bind(Self::id(path))
        .fetch_one(self.db.pool())
        .await
        .expect("row")
    }

    async fn set(&self, path: &Path, column: &str, value: Option<String>) {
        sqlx::query(&format!(
            r#"UPDATE "BaseItems" SET "{column}" = ?1 WHERE "Id" = ?2"#
        ))
        .bind(value)
        .bind(Self::id(path))
        .execute(self.db.writer())
        .await
        .expect("update");
        reset_writes(&self.db).await;
    }

    /// Rows written (inserted, updated or deleted) per table since the last
    /// call.
    async fn writes(&self) -> HashMap<String, i64> {
        let rows: Vec<(String, i64)> =
            sqlx::query_as(r#"SELECT "Tbl", "N" FROM "TestWrites" WHERE "N" > 0"#)
                .fetch_all(self.db.pool())
                .await
                .expect("writes");
        reset_writes(&self.db).await;
        rows.into_iter().collect()
    }
}

/// Counts every row written to every table from here on.
async fn count_writes(db: &Database) {
    sqlx::query(r#"CREATE TABLE "TestWrites" ("Tbl" TEXT PRIMARY KEY, "N" INTEGER NOT NULL)"#)
        .execute(db.writer())
        .await
        .expect("counter table");
    let tables: Vec<String> = sqlx::query_scalar(
        r#"SELECT "name" FROM sqlite_master WHERE "type" = 'table'
             AND "name" NOT LIKE 'sqlite_%' AND "name" <> 'TestWrites'"#,
    )
    .fetch_all(db.pool())
    .await
    .expect("tables");
    for table in tables {
        sqlx::query(r#"INSERT INTO "TestWrites" VALUES (?1, 0)"#)
            .bind(&table)
            .execute(db.writer())
            .await
            .expect("counter row");
        for event in ["INSERT", "UPDATE", "DELETE"] {
            sqlx::query(&format!(
                r#"CREATE TRIGGER "TestWrites_{table}_{event}" AFTER {event} ON "{table}"
                   BEGIN UPDATE "TestWrites" SET "N" = "N" + 1 WHERE "Tbl" = '{table}'; END"#
            ))
            .execute(db.writer())
            .await
            .expect("trigger");
        }
    }
}

async fn reset_writes(db: &Database) {
    sqlx::query(r#"UPDATE "TestWrites" SET "N" = 0"#)
        .execute(db.writer())
        .await
        .expect("reset");
}

/// Moves `path`'s mtime `seconds` from now.
fn touch(path: &Path, seconds: i64) {
    let file = std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open");
    let now = std::time::SystemTime::now();
    let at = if seconds >= 0 {
        now + std::time::Duration::from_secs(seconds.unsigned_abs())
    } else {
        now - std::time::Duration::from_secs(seconds.unsigned_abs())
    };
    file.set_modified(at).expect("set mtime");
}

fn db_time(at: chrono::DateTime<chrono::Utc>) -> String {
    ferrofin_db::store::datetime_to_db(at)
}

/// Everything the first scan does, done; the counters are then reset.
async fn scanned_once(tmp: &Path, interval_days: i32) -> Fixture {
    let fx = Fixture::new(tmp, interval_days).await;
    assert_eq!(
        fx.scan().await,
        ScanOutcome {
            created: 2,
            ..ScanOutcome::default()
        }
    );
    assert_eq!(
        fx.probe.take(),
        ["Heat (1995).mkv", "The Matrix (1999).mkv"]
    );
    assert!(!fx.tmdb.take().is_empty(), "a new item asks its providers");
    let _ = fx.writes().await;
    fx
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unchanged_rescan_probes_nothing_fetches_nothing_and_writes_nothing() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = scanned_once(tmp.path(), 0).await;
    let before = (fx.stamps(&fx.matrix).await, fx.stamps(&fx.heat).await);
    assert!(
        before.0.0.is_some(),
        "a new item's first refresh stamps DateLastSaved"
    );
    assert!(before.0.1.is_some(), "and DateLastRefreshed");

    assert_eq!(
        fx.scan().await,
        ScanOutcome {
            unchanged: 2,
            ..ScanOutcome::default()
        }
    );
    assert!(fx.probe.take().is_empty(), "no ffprobe");
    assert_eq!(fx.tmdb.take(), Vec::<String>::new(), "no provider request");
    let writes = fx.writes().await;
    assert!(writes.is_empty(), "no row written anywhere: {writes:?}");
    assert_eq!(
        (fx.stamps(&fx.matrix).await, fx.stamps(&fx.heat).await),
        before,
        "DateLastSaved (the Etag input) and DateLastRefreshed stay put"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_touched_file_is_probed_fetched_and_saved_alone() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = scanned_once(tmp.path(), 0).await;
    let heat_before = fx.stamps(&fx.heat).await;
    let matrix_before = fx.stamps(&fx.matrix).await;

    touch(&fx.matrix, 3_600);
    assert_eq!(
        fx.scan().await,
        ScanOutcome {
            updated: 1,
            unchanged: 1,
            ..ScanOutcome::default()
        }
    );
    assert_eq!(fx.probe.take(), ["The Matrix (1999).mkv"]);
    let requests = fx.tmdb.take();
    assert!(
        requests.iter().any(|r| r.contains("/movie/603?")),
        "the changed item runs its remote providers: {requests:?}"
    );
    assert!(
        !requests
            .iter()
            .any(|r| r.contains("Heat") || r.contains("/movie/949")),
        "the unchanged one does not: {requests:?}"
    );
    assert_eq!(fx.stamps(&fx.heat).await, heat_before);
    assert_ne!(fx.stamps(&fx.matrix).await.0, matrix_before.0, "saved");

    // And it is quiet again.
    assert_eq!(fx.scan().await.unchanged, 2);
    assert!(fx.probe.take().is_empty());
    assert!(fx.tmdb.take().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_nfo_newer_than_the_last_save_rereads_only_that_items_local_metadata() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = scanned_once(tmp.path(), 0).await;
    // The last save was ten minutes ago, the NFO is written now: newer by
    // more than `BaseNfoProvider`'s one-minute tolerance.
    fx.set(
        &fx.heat,
        "DateLastSaved",
        Some(db_time(chrono::Utc::now() - chrono::TimeDelta::minutes(10))),
    )
    .await;
    let nfo = fx.heat.with_extension("nfo");
    std::fs::write(
        &nfo,
        "<movie><title>Heat</title><plot>A thief and a detective.</plot></movie>",
    )
    .expect("nfo");
    let matrix_before = fx.stamps(&fx.matrix).await;

    assert_eq!(
        fx.scan().await,
        ScanOutcome {
            updated: 1,
            unchanged: 1,
            ..ScanOutcome::default()
        }
    );
    assert!(fx.probe.take().is_empty(), "the NFO needs no probe");
    assert!(fx.tmdb.take().is_empty(), "and no remote provider");
    let overview: Option<String> =
        sqlx::query_scalar(r#"SELECT "Overview" FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(Fixture::id(&fx.heat))
            .fetch_one(fx.db.pool())
            .await
            .expect("overview");
    assert_eq!(overview.as_deref(), Some("A thief and a detective."));
    assert_eq!(fx.stamps(&fx.matrix).await, matrix_before);

    // Saving moved DateLastSaved past the NFO: quiet again.
    assert_eq!(fx.scan().await.unchanged, 2);

    // An NFO written within a minute of the last save is our own write.
    touch(&nfo, 30);
    assert_eq!(fx.scan().await.unchanged, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_sidecar_subtitle_reprobes_only_that_video() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = scanned_once(tmp.path(), 0).await;
    let sidecar = fx.heat.with_file_name("Heat (1995).eng.srt");
    std::fs::write(&sidecar, b"1\n").expect("srt");

    assert_eq!(
        fx.scan().await,
        ScanOutcome {
            updated: 1,
            unchanged: 1,
            ..ScanOutcome::default()
        }
    );
    assert_eq!(fx.probe.take(), ["Heat (1995).eng.srt", "Heat (1995).mkv"]);
    assert!(
        fx.tmdb.take().is_empty(),
        "a sidecar is not a reason to refetch"
    );
    let external: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM "MediaStreamInfos" WHERE "ItemId" = ?1 AND "IsExternal" = 1"#,
    )
    .bind(Fixture::id(&fx.heat))
    .fetch_one(fx.db.pool())
    .await
    .expect("streams");
    assert_eq!(external, 1);

    // The stored streams now name the sidecar: quiet again.
    assert_eq!(fx.scan().await.unchanged, 2);
    assert!(fx.probe.take().is_empty());

    // Removing it is a change too.
    std::fs::remove_file(&sidecar).expect("rm");
    assert_eq!(fx.scan().await.updated, 1);
    assert_eq!(fx.probe.take(), ["Heat (1995).mkv"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_never_refreshed_item_runs_everything_once_then_goes_quiet() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = scanned_once(tmp.path(), 0).await;
    // What every row of a database scanned before this change looks like.
    fx.set(&fx.heat, "DateLastRefreshed", None).await;

    assert_eq!(
        fx.scan().await,
        ScanOutcome {
            updated: 1,
            unchanged: 1,
            ..ScanOutcome::default()
        }
    );
    assert_eq!(fx.probe.take(), ["Heat (1995).mkv"]);
    assert!(
        fx.tmdb.take().iter().any(|r| r.contains("/movie/949?")),
        "a first refresh runs every provider"
    );
    assert!(fx.stamps(&fx.heat).await.1.is_some(), "and stamps the row");

    assert_eq!(fx.scan().await.unchanged, 2);
    assert!(fx.probe.take().is_empty());
    assert!(fx.tmdb.take().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_elapsed_refresh_interval_refetches() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = scanned_once(tmp.path(), 30).await;
    assert_eq!(fx.scan().await.unchanged, 2, "not yet elapsed");
    assert!(fx.tmdb.take().is_empty());

    fx.set(
        &fx.matrix,
        "DateLastRefreshed",
        Some(db_time(chrono::Utc::now() - chrono::TimeDelta::days(31))),
    )
    .await;
    assert_eq!(
        fx.scan().await,
        ScanOutcome {
            updated: 1,
            unchanged: 1,
            ..ScanOutcome::default()
        }
    );
    let requests = fx.tmdb.take();
    assert!(
        requests.iter().any(|r| r.contains("/movie/603?")),
        "{requests:?}"
    );
    assert_eq!(fx.probe.take(), ["The Matrix (1999).mkv"]);
    assert_eq!(fx.scan().await.unchanged, 2, "the refetch restamped it");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_provider_is_retried_but_one_that_found_nothing_is_not() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = Fixture::new(tmp.path(), 0).await;
    fx.tmdb.answer_heat(Answer::Fail);
    assert_eq!(fx.scan().await.created, 2);
    assert!(
        fx.stamps(&fx.heat).await.1.is_none(),
        "a refresh with a provider failure is not stamped"
    );
    assert!(fx.stamps(&fx.matrix).await.1.is_some());
    let _ = (fx.probe.take(), fx.tmdb.take());

    // Still failing: retried (as a first refresh), the other item quiet.
    let outcome = fx.scan().await;
    assert_eq!((outcome.updated, outcome.unchanged), (1, 1));
    let requests = fx.tmdb.take();
    assert!(!requests.is_empty() && requests.iter().all(|r| r.contains("Heat")));
    assert!(fx.stamps(&fx.heat).await.1.is_none());

    // TMDB now answers that it has nothing: that completes the refresh.
    fx.tmdb.answer_heat(Answer::Nothing);
    assert_eq!(fx.scan().await.updated, 1);
    let stamped = fx.stamps(&fx.heat).await;
    assert!(stamped.1.is_some(), "found nothing still stamps");
    let _ = fx.tmdb.take();

    // Upstream would never ask again. Ferrofin's kept backfill heuristic
    // (owner decision D2: no overview → ask TMDB) still does, on the stored
    // row — but an answer of nothing changes nothing, so nothing is written.
    assert_eq!(fx.scan().await.unchanged, 2);
    let requests = fx.tmdb.take();
    assert!(
        !requests.is_empty()
            && requests
                .iter()
                .all(|r| r.contains("/search/movie?") && r.contains("Heat")),
        "only the backfill's search for the title with no overview: {requests:?}"
    );
    assert_eq!(fx.stamps(&fx.heat).await, stamped);
}

/// A music album with a cover beside its tracks stores its shared,
/// content-addressed cover — the loop and the post-scan album pass agree on
/// it, so an unchanged rescan rewrites neither the album's images nor
/// anything else.
#[tokio::test(flavor = "multi_thread")]
async fn an_unchanged_album_with_a_cover_is_not_rewritten() {
    let tmp = tempfile::tempdir().expect("tmp");
    let music = tmp.path().join("music");
    let album = music.join("Great Winds").join("The Sour Kingdom (1996)");
    std::fs::create_dir_all(&album).expect("mkdir");
    std::fs::write(album.join("01 - Opening.mp3"), b"0123").expect("track");
    std::fs::write(album.join("cover.jpg"), b"\xFF\xD8\xFFcover").expect("cover");
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(tmp.path().join("views"))
            .with_item_store(persistence.clone()),
    );
    vf.add_virtual_folder(
        "Music",
        Some(CollectionTypeOptions::music),
        &LibraryOptions {
            path_infos: vec![MediaPathInfo {
                path: music.to_string_lossy().into_owned(),
            }],
            ..LibraryOptions::default()
        },
    )
    .await
    .expect("add library");
    let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
        .with_items(Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        )))
        .with_probe(
            Arc::new(RecordingProbe::default()) as Arc<dyn MediaEncoder>,
            Arc::new(FerrofinMediaStreamRepository::new(db.clone()))
                as Arc<dyn MediaStreamRepository>,
            Arc::new(FerrofinChapterRepository::new(db.clone())),
        )
        .with_metadata_dir(tmp.path().join("metadata"))
        .with_progress_every(0);
    count_writes(&db).await;
    let first = scanner.scan_all().await.expect("scan");
    assert!(first.created >= 2, "{first:?}");
    reset_writes(&db).await;

    let rescan = scanner.scan_all().await.expect("rescan");
    assert_eq!(rescan.unchanged, first.created, "{rescan:?}");
    let writes: Vec<(String, i64)> =
        sqlx::query_as(r#"SELECT "Tbl", "N" FROM "TestWrites" WHERE "N" > 0"#)
            .fetch_all(db.pool())
            .await
            .expect("writes");
    assert!(writes.is_empty(), "no row written anywhere: {writes:?}");
}

/// Scans `media` as a library of `kind` with the probe stand-in, the item and
/// people repositories, and (when `tmdb` is given) TMDB; counts every row
/// written from here on.
async fn library(
    root: &Path,
    media: &Path,
    kind: CollectionTypeOptions,
    tmdb: Option<&str>,
) -> (Database, LibraryScanner) {
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrate");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    let vf: Arc<dyn VirtualFolderManager> = Arc::new(
        FerrofinVirtualFolderManager::new(root.join("views")).with_item_store(persistence.clone()),
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
    let mut scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), persistence)
        .with_items(Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        )))
        .with_people(Arc::new(ferrofin_core::FerrofinPeopleRepository::new(
            db.clone(),
        )))
        .with_probe(
            Arc::new(RecordingProbe::default()) as Arc<dyn MediaEncoder>,
            Arc::new(FerrofinMediaStreamRepository::new(db.clone()))
                as Arc<dyn MediaStreamRepository>,
            Arc::new(FerrofinChapterRepository::new(db.clone())),
        )
        .with_metadata_dir(root.join("metadata"))
        .with_progress_every(0);
    if let Some(base) = tmdb {
        scanner = scanner.with_metadata(
            Arc::new(TmdbClient::new().with_base_url(base)),
            root.join("metadata"),
        );
    }
    count_writes(&db).await;
    (db, scanner)
}

async fn written(db: &Database) -> Vec<(String, i64)> {
    let rows = sqlx::query_as(r#"SELECT "Tbl", "N" FROM "TestWrites" WHERE "N" > 0"#)
        .fetch_all(db.pool())
        .await
        .expect("writes");
    reset_writes(db).await;
    rows
}

/// A TMDB stand-in for The Matrix with NO trailers (so the kept D2
/// `wants_trailers` backfill asks again on every scan), a synopsis and a
/// cast that differ from the NFO's. Returns the base URL and a request count.
fn spawn_trailerless_tmdb() -> (String, Arc<Mutex<usize>>) {
    spawn_trailerless_tmdb_with(r#"[{"id": 7, "name": "Tmdb Actor", "character": "Neo"}]"#)
}

/// [`spawn_trailerless_tmdb`] with the given `credits.cast` JSON array.
fn spawn_trailerless_tmdb_with(cast: &'static str) -> (String, Arc<Mutex<usize>>) {
    let requests = Arc::new(Mutex::new(0));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let counter = Arc::clone(&requests);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap_or(0);
            let line = String::from_utf8_lossy(&buf[..n])
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned();
            *counter.lock().expect("lock") += 1;
            let (status, payload) = if line.contains("/search/movie") {
                (
                    "200 OK",
                    r#"{"results": [{"id": 603, "title": "The Matrix"}]}"#.to_owned(),
                )
            } else if line.contains("/movie/603?") {
                (
                    "200 OK",
                    format!(
                        r#"{{"title": "The Matrix", "overview": "TMDB's synopsis.",
                        "genres": [{{"name": "Action"}}], "credits": {{"cast": {cast}}}}}"#
                    ),
                )
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
    (format!("http://{addr}"), requests)
}

/// The D2 backfill runs the local readers with the remote providers
/// (`MetadataService.cs:689-693`): an NFO's synopsis and cast are kept over
/// TMDB's, and a backfill pass that changes nothing writes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_backfill_pass_keeps_the_nfo_and_writes_nothing_when_nothing_changed() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    let file = media
        .join("The Matrix (1999)")
        .join("The Matrix (1999).mkv");
    std::fs::create_dir_all(file.parent().expect("dir")).expect("mkdir");
    std::fs::write(&file, b"0123").expect("write");
    std::fs::write(
        file.with_extension("nfo"),
        "<movie><title>The Matrix</title><plot>The NFO's synopsis.</plot>\
         <actor><name>Nfo Actor</name><role>Neo</role></actor></movie>",
    )
    .expect("nfo");
    let (base, requests) = spawn_trailerless_tmdb();
    let (db, scanner) = library(
        tmp.path(),
        &media,
        CollectionTypeOptions::movies,
        Some(&base),
    )
    .await;

    let overview_and_cast = || async {
        let id = Fixture::id(&file);
        let overview: Option<String> =
            sqlx::query_scalar(r#"SELECT "Overview" FROM "BaseItems" WHERE "Id" = ?1"#)
                .bind(&id)
                .fetch_one(db.pool())
                .await
                .expect("overview");
        let cast: Vec<String> = sqlx::query_scalar(
            r#"SELECT p."Name" FROM "PeopleBaseItemMap" m JOIN "Peoples" p ON p."Id" = m."PeopleId"
               WHERE m."ItemId" = ?1 ORDER BY m."ListOrder""#,
        )
        .bind(&id)
        .fetch_all(db.pool())
        .await
        .expect("cast");
        (overview, cast)
    };

    assert_eq!(scanner.scan_all().await.expect("scan").created, 1);
    let first = overview_and_cast().await;
    assert_eq!(first.0.as_deref(), Some("The NFO's synopsis."));
    assert_eq!(first.1, ["Nfo Actor"]);
    let _ = written(&db).await;
    let asked = *requests.lock().expect("lock");

    assert_eq!(
        scanner.scan_all().await.expect("rescan"),
        ScanOutcome {
            unchanged: 1,
            ..ScanOutcome::default()
        }
    );
    assert!(
        *requests.lock().expect("lock") > asked,
        "no trailers: the backfill asked TMDB again"
    );
    assert_eq!(
        overview_and_cast().await,
        first,
        "the NFO's values are kept"
    );
    assert_eq!(written(&db).await, Vec::<(String, i64)>::new());
}

/// Local image validation on an unchanged item: a new or replaced
/// `poster.jpg` is picked up and saved; a deleted one is removed.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_replaced_or_deleted_poster_is_validated_on_an_unchanged_item() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    let file = media.join("Heat (1995)").join("Heat (1995).mkv");
    std::fs::create_dir_all(file.parent().expect("dir")).expect("mkdir");
    std::fs::write(&file, b"0123").expect("write");
    let (db, scanner) = library(tmp.path(), &media, CollectionTypeOptions::movies, None).await;
    let images = || async {
        sqlx::query_as::<_, (String, Option<String>)>(
            r#"SELECT "Path", "DateModified" FROM "BaseItemImageInfos" WHERE "ItemId" = ?1"#,
        )
        .bind(Fixture::id(&file))
        .fetch_all(db.pool())
        .await
        .expect("images")
    };
    assert_eq!(scanner.scan_all().await.expect("scan").created, 1);
    assert!(images().await.is_empty());

    let poster = file.with_file_name("poster.jpg");
    std::fs::write(&poster, b"\xFF\xD8\xFFposter").expect("poster");
    assert_eq!(scanner.scan_all().await.expect("rescan").updated, 1);
    let found = images().await;
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].0.ends_with("poster.jpg"));
    let _ = written(&db).await;
    assert_eq!(scanner.scan_all().await.expect("rescan").unchanged, 1);
    assert_eq!(written(&db).await, Vec::<(String, i64)>::new());

    touch(&poster, 3_600);
    assert_eq!(scanner.scan_all().await.expect("rescan").updated, 1);
    assert_ne!(images().await[0].1, found[0].1, "the replacement's mtime");

    std::fs::remove_file(&poster).expect("rm");
    assert_eq!(scanner.scan_all().await.expect("rescan").updated, 1);
    assert!(
        images().await.is_empty(),
        "a vanished image's row is removed"
    );
    assert_eq!(scanner.scan_all().await.expect("rescan").unchanged, 1);
}

/// A new episode in an existing season: it is created, its season (whose
/// directory mtime moved) is refreshed, and its series and sibling are left
/// alone.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_episode_refreshes_its_season_and_leaves_its_siblings_alone() {
    let tmp = tempfile::tempdir().expect("tmp");
    let tv = tmp.path().join("tv");
    let season = tv.join("Show").join("Season 1");
    std::fs::create_dir_all(&season).expect("mkdir");
    std::fs::write(season.join("Show S01E01.mkv"), b"0123").expect("write");
    let (db, scanner) = library(tmp.path(), &tv, CollectionTypeOptions::tvshows, None).await;
    assert_eq!(scanner.scan_all().await.expect("scan").created, 3);
    let saved = |path: PathBuf, kind: BaseItemKind| {
        let db = db.clone();
        async move {
            sqlx::query_scalar::<_, Option<String>>(
                r#"SELECT "DateLastSaved" FROM "BaseItems" WHERE "Id" = ?1"#,
            )
            .bind(guid_to_db(
                derive_item_id(kind, &path.to_string_lossy()).expect("id"),
            ))
            .fetch_one(db.pool())
            .await
            .expect("row")
        }
    };
    let sibling = season.join("Show S01E01.mkv");
    let before = (
        saved(tv.join("Show"), BaseItemKind::Series).await,
        saved(sibling.clone(), BaseItemKind::Episode).await,
        saved(season.clone(), BaseItemKind::Season).await,
    );
    // The directory mtime moves by whole seconds only on some filesystems;
    // make the drift unambiguous.
    std::fs::write(season.join("Show S01E02.mkv"), b"0123").expect("new episode");
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(3_600);
    std::fs::File::open(&season)
        .expect("season dir")
        .set_modified(later)
        .expect("touch dir");

    assert_eq!(
        scanner.scan_all().await.expect("rescan"),
        ScanOutcome {
            created: 1,
            updated: 1,
            unchanged: 2,
            ..ScanOutcome::default()
        }
    );
    assert_eq!(saved(tv.join("Show"), BaseItemKind::Series).await, before.0);
    assert_eq!(saved(sibling, BaseItemKind::Episode).await, before.1);
    assert_ne!(saved(season, BaseItemKind::Season).await, before.2);
}

/// A locked item: an unchanged rescan leaves it alone like any other; a
/// changed file still refreshes its file facts (the probe is a forced
/// provider) but no remote provider runs for it.
#[tokio::test(flavor = "multi_thread")]
async fn a_locked_item_is_quiet_and_asks_no_provider() {
    let tmp = tempfile::tempdir().expect("tmp");
    let fx = scanned_once(tmp.path(), 0).await;
    sqlx::query(r#"UPDATE "BaseItems" SET "IsLocked" = 1 WHERE "Id" = ?1"#)
        .bind(Fixture::id(&fx.heat))
        .execute(fx.db.writer())
        .await
        .expect("lock");
    let _ = fx.writes().await;
    assert_eq!(fx.scan().await.unchanged, 2);
    assert!(fx.writes().await.is_empty());

    touch(&fx.heat, 3_600);
    assert_eq!(fx.scan().await.updated, 1);
    assert_eq!(fx.probe.take(), ["Heat (1995).mkv"]);
    assert!(
        fx.tmdb.take().is_empty(),
        "no remote provider runs for a locked item"
    );
    assert_eq!(fx.scan().await.unchanged, 2);
}

/// A backfill pass compares the cast the way `update_people` writes it:
/// deduped on (name, type), names trimmed and matched case-insensitively.
/// TMDB credits one actor in two roles all the time; that must not read as a
/// changed cast (and rewrite the title) on every scan.
#[tokio::test(flavor = "multi_thread")]
async fn a_cast_with_one_person_in_two_roles_is_unchanged_on_a_backfill_pass() {
    let tmp = tempfile::tempdir().expect("tmp");
    let media = tmp.path().join("movies");
    let file = media
        .join("The Matrix (1999)")
        .join("The Matrix (1999).mkv");
    std::fs::create_dir_all(file.parent().expect("dir")).expect("mkdir");
    std::fs::write(&file, b"0123").expect("write");
    let (base, requests) = spawn_trailerless_tmdb_with(
        r#"[{"id": 7, "name": "Tmdb Actor", "character": "Neo"},
            {"id": 7, "name": "Tmdb Actor", "character": "Thomas Anderson"},
            {"id": 8, "name": " tmdb actor ", "character": "Echo"},
            {"id": 9, "name": "Other Actor", "character": "Trinity"}]"#,
    );
    let (db, scanner) = library(
        tmp.path(),
        &media,
        CollectionTypeOptions::movies,
        Some(&base),
    )
    .await;
    assert_eq!(scanner.scan_all().await.expect("scan").created, 1);
    let _ = written(&db).await;
    let asked = *requests.lock().expect("lock");

    assert_eq!(
        scanner.scan_all().await.expect("rescan"),
        ScanOutcome {
            unchanged: 1,
            ..ScanOutcome::default()
        }
    );
    assert!(
        *requests.lock().expect("lock") > asked,
        "the backfill asked again"
    );
    assert_eq!(written(&db).await, Vec::<(String, i64)>::new());
}
