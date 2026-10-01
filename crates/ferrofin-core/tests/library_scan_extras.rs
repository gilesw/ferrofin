//! Extras stay owned by their movie and outside library browse (#32).

use std::path::PathBuf;
use std::sync::Arc;

use ferrofin_core::file_system::FerrofinFileSystem;
use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::{
    FerrofinItemPersistenceService, FerrofinItemRepository, FerrofinVirtualFolderManager,
    LibraryScanner,
};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo};
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_traits::library::VirtualFolderManager;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::{ItemPersistenceService, ItemRepository};
use uuid::Uuid;

struct Fixture {
    tmp: tempfile::TempDir,
    media: PathBuf,
    db: Database,
    repo: Arc<FerrofinItemRepository>,
    store: Arc<FerrofinItemPersistenceService>,
    scanner: LibraryScanner,
    library: Uuid,
    fs: Arc<SelectiveFs>,
}

impl Fixture {
    async fn new(files: &[&str]) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("movies");
        for file in files {
            let path = media.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"").unwrap();
        }
        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let store = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
        let vf: Arc<dyn VirtualFolderManager> = Arc::new(
            FerrofinVirtualFolderManager::new(tmp.path().join("views"))
                .with_item_store(store.clone()),
        );
        vf.add_virtual_folder(
            "Movies",
            Some(CollectionTypeOptions::movies),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: media.to_string_lossy().into_owned(),
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let library = Uuid::parse_str(
            vf.get_virtual_folders().await.unwrap()[0]
                .item_id
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        let repo = Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        ));
        let fs = Arc::new(SelectiveFs::default());
        let scanner = LibraryScanner::new(vf, fs.clone(), store.clone()).with_items(repo.clone());
        Self {
            tmp,
            media,
            db,
            repo,
            store,
            scanner,
            library,
            fs,
        }
    }

    async fn row(&self, relative: &str) -> BaseItemEntity {
        sqlx::query_as(r#"SELECT * FROM "BaseItems" WHERE "Path" = ?"#)
            .bind(self.media.join(relative).to_string_lossy().as_ref())
            .fetch_one(self.db.pool())
            .await
            .unwrap()
    }

    async fn assert_browse(&self, expected: usize) {
        for recursive in [false, true] {
            let rows = self
                .repo
                .get_item_list(&InternalItemsQuery {
                    parent_id: self.library,
                    recursive,
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!(rows.len(), expected, "recursive={recursive}: {rows:?}");
            assert!(
                rows.iter().all(|r| r.owner_id.is_none()),
                "owned extra in browse"
            );
        }
    }
}

const MOVIE: &str = "Heat (1995)/Heat.mkv";
const EXTRA: &str = "Heat (1995)/Extras/Deleted.Scenes.avi";

#[tokio::test]
async fn owned_extras_are_accessible_without_becoming_library_children() {
    let f = Fixture::new(&[
        MOVIE,
        EXTRA,
        "Heat (1995)/Heat-trailer.mkv",
        "Heat (1995)/theme.mp3",
        "Heat (1995)/Heat-sample.mkv",
    ])
    .await;
    f.scanner.scan_all().await.unwrap();
    f.assert_browse(1).await;
    let owner = Uuid::parse_str(&f.row(MOVIE).await.id).unwrap();
    let extras = f
        .repo
        .get_item_list(&InternalItemsQuery {
            owner_ids: vec![owner],
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(extras.len(), 4);
    assert!(
        extras
            .iter()
            .all(|r| r.parent_id.is_none() && r.top_parent_id.is_none())
    );
    for extra in extras {
        assert!(
            f.repo
                .retrieve_item(Uuid::parse_str(&extra.id).unwrap())
                .await
                .unwrap()
                .is_some()
        );
    }
}

#[tokio::test]
async fn normal_scan_repairs_locked_unchanged_extra_relationships() {
    let f = Fixture::new(&[MOVIE, EXTRA]).await;
    f.scanner.scan_all().await.unwrap();
    let mut extra = f.row(EXTRA).await;
    let id = extra.id.clone();
    extra.parent_id = Some(guid_to_db(f.library));
    extra.top_parent_id = Some(guid_to_db(f.library));
    extra.is_locked = true;
    extra.overview = Some("Preserve this description".into());
    f.store
        .save_items(std::slice::from_ref(&extra))
        .await
        .unwrap();
    play(&f.db, &id).await;
    let poster = f.media.join("extra-poster.png");
    std::fs::write(&poster, b"retained image").unwrap();
    sqlx::query(r#"INSERT INTO "BaseItemImageInfos" ("Id", "ItemId", "Path", "ImageType", "Width", "Height") VALUES (?, ?, ?, 0, 100, 150)"#)
        .bind(guid_to_db(Uuid::new_v4())).bind(&id).bind(poster.to_string_lossy().as_ref()).execute(f.db.pool()).await.unwrap();
    sqlx::query(r#"INSERT INTO "BaseItemProviders" ("ItemId", "ProviderId", "ProviderValue") VALUES (?, 'Imdb', 'tt12345')"#)
        .bind(&id).execute(f.db.pool()).await.unwrap();
    f.scanner.scan_all().await.unwrap();
    f.assert_browse(1).await;
    let history: (i64, i64) =
        sqlx::query_as(r#"SELECT "Played", "PlayCount" FROM "UserData" WHERE "ItemId"=?"#)
            .bind(&id)
            .fetch_one(f.db.pool())
            .await
            .unwrap();
    assert_eq!(history, (1, 1));
    let image: String =
        sqlx::query_scalar(r#"SELECT "Path" FROM "BaseItemImageInfos" WHERE "ItemId"=?"#)
            .bind(&id)
            .fetch_one(f.db.pool())
            .await
            .unwrap();
    assert_eq!(image, poster.to_string_lossy());
    let provider: String = sqlx::query_scalar(r#"SELECT "ProviderValue" FROM "BaseItemProviders" WHERE "ItemId"=? AND "ProviderId"='Imdb'"#).bind(&id).fetch_one(f.db.pool()).await.unwrap();
    assert_eq!(provider, "tt12345");
    let repaired = f.row(EXTRA).await;
    assert_eq!(repaired.id, id);
    assert_eq!(repaired.overview, extra.overview);
    assert!(repaired.is_locked);
    assert!(repaired.parent_id.is_none() && repaired.top_parent_id.is_none());
    let saved = repaired.date_last_saved;
    f.scanner.scan_all().await.unwrap();
    assert_eq!(f.row(EXTRA).await.date_last_saved, saved);
}

#[tokio::test]
async fn scan_ignores_resource_forks_and_dot_samples_but_keeps_owned_suffix_samples() {
    let f = Fixture::new(&[
        MOVIE,
        "Heat (1995)/._Heat.mkv",
        "Heat (1995)/sample.mkv",
        "Heat (1995)/Heat.sample.mkv",
        "Heat (1995)/Heat-sample.mkv",
        "Heat (1995)/Heat_sample.mkv",
        "Heat (1995)/samples/clip.mkv",
        "@eaDir/stray.mkv",
        ".hidden/stray.mkv",
    ])
    .await;
    f.scanner.scan_all().await.unwrap();
    let paths: Vec<String> = sqlx::query_scalar(
        r#"SELECT "Path" FROM "BaseItems" WHERE "MediaType" IS NOT NULL ORDER BY "Path""#,
    )
    .fetch_all(f.db.pool())
    .await
    .unwrap();
    assert_eq!(paths.len(), 4, "{paths:?}");
    assert_eq!(
        f.row("Heat (1995)/Heat-sample.mkv").await.extra_type,
        Some(7)
    );
    assert_eq!(
        f.row("Heat (1995)/Heat_sample.mkv").await.extra_type,
        Some(7)
    );
    assert_eq!(
        f.row("Heat (1995)/samples/clip.mkv").await.extra_type,
        Some(7)
    );
}

#[tokio::test]
async fn removed_parentless_extras_are_pruned_by_full_and_scoped_scans() {
    for scoped in [false, true] {
        let f = Fixture::new(&[MOVIE, EXTRA]).await;
        f.scanner.scan_all().await.unwrap();
        let extra = f.row(EXTRA).await;
        let path = f.media.join(EXTRA);
        std::fs::remove_file(&path).unwrap();
        let result = if scoped {
            f.scanner
                .scan_paths(&[path.to_string_lossy().into_owned()])
                .await
                .unwrap()
        } else {
            f.scanner.scan_all().await.unwrap()
        };
        assert_eq!(result.removed, 1);
        assert!(
            f.repo
                .retrieve_item(Uuid::parse_str(&extra.id).unwrap())
                .await
                .unwrap()
                .is_none()
        );
        f.assert_browse(1).await;
    }
}

#[tokio::test]
async fn deleting_the_owner_removes_parentless_extras() {
    let f = Fixture::new(&[MOVIE, EXTRA]).await;
    f.scanner.scan_all().await.unwrap();
    let movie = Uuid::parse_str(&f.row(MOVIE).await.id).unwrap();
    let extra = Uuid::parse_str(&f.row(EXTRA).await.id).unwrap();
    f.store.delete_items(&[movie]).await.unwrap();
    assert!(f.repo.retrieve_item(extra).await.unwrap().is_none());
    assert!(
        f.media.join(EXTRA).exists(),
        "database deletion keeps the file"
    );
}

#[tokio::test]
async fn nested_release_and_mixed_folders_do_not_lend_the_wrong_owner() {
    let f = Fixture::new(&[
        MOVIE,
        EXTRA,
        "Heat (1995)/Heat-sample.mkv",
        "Heat (1995)/Release/Other.mkv",
        "Heat (1995)/Release/other-sample.mkv",
        "Mixed/First.mkv",
        "Mixed/Second.mkv",
        "Mixed/First-trailer.mkv",
    ])
    .await;
    f.scanner.scan_all().await.unwrap();
    let extras = f
        .repo
        .get_item_list(&InternalItemsQuery {
            has_owner_id: Some(true),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(extras.len(), 1);
    assert_eq!(
        extras[0].owner_id,
        Some(f.row("Heat (1995)/Release/Other.mkv").await.id)
    );
    assert!(f.row(MOVIE).await.is_in_mixed_folder);
    assert!(
        !f.row("Heat (1995)/Release/Other.mkv")
            .await
            .is_in_mixed_folder
    );
}

#[tokio::test]
async fn scoped_discovery_ignores_files_and_preserves_metadata_sidecars() {
    let f = Fixture::new(&[MOVIE]).await;
    let nfo = f.media.join("Heat (1995)/movie.nfo");
    std::fs::write(
        &nfo,
        "<movie><title>Local title</title><plot>Local plot</plot></movie>",
    )
    .unwrap();
    f.scanner.scan_all().await.unwrap();
    assert_eq!(f.row(MOVIE).await.overview.as_deref(), Some("Local plot"));
    for path in [
        "Heat (1995)/._Heat.mkv",
        "Heat (1995)/sample.mkv",
        ".hidden/stray.mkv",
    ] {
        let path = f.media.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"").unwrap();
        let result = f
            .scanner
            .scan_paths(&[path.to_string_lossy().into_owned()])
            .await
            .unwrap();
        assert_eq!(result.created, 0);
    }
    f.assert_browse(1).await;
    assert_eq!(f.row(MOVIE).await.overview.as_deref(), Some("Local plot"));
}

#[tokio::test]
async fn grouped_movies_keep_generic_and_version_specific_extras_in_full_and_scoped_scans() {
    for scoped in [false, true] {
        for (movies, owners) in [
            (
                vec![
                    "Film/Film.mkv",
                    "Film/Film - 4K.mkv",
                    "Film/Film - 4Kish.mkv",
                ],
                vec![
                    "Film/Film.mkv",
                    "Film/Film - 4K.mkv",
                    "Film/Film - 4Kish.mkv",
                ],
            ),
            (
                vec!["Film/Film cd1.mkv", "Film/Film cd2.mkv"],
                vec!["Film/Film cd1.mkv"; 3],
            ),
        ] {
            let f = Fixture::new(&movies).await;
            f.scanner.scan_all().await.unwrap();
            let extras = [
                "Film/Extras/clip.mkv",
                "Film/Film - 4K-trailer.mkv",
                "Film/Film - 4Kish-trailer.mkv",
            ];
            for (extra, owner) in extras.into_iter().zip(owners) {
                let path = f.media.join(extra);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, b"").unwrap();
                if scoped {
                    f.scanner
                        .scan_paths(&[path.to_string_lossy().into_owned()])
                        .await
                        .unwrap();
                } else {
                    f.scanner.scan_all().await.unwrap();
                }
                let extra = f.row(extra).await;
                assert_eq!(extra.owner_id, Some(f.row(owner).await.id));
                assert!(extra.parent_id.is_none());
            }
        }
    }
}

#[tokio::test]
async fn adopted_version_identity_is_reused_for_extra_owners_even_outside_scan_scope() {
    for scoped in [false, true] {
        let f = Fixture::new(&["Film/Film.mkv", "Film/Film - 4K.mkv"]).await;
        f.scanner.scan_all().await.unwrap();
        let mut version = f.row("Film/Film - 4K.mkv").await;
        let adopted_id = guid_to_db(Uuid::new_v4());
        f.store
            .delete_items(&[Uuid::parse_str(&version.id).unwrap()])
            .await
            .unwrap();
        version.id.clone_from(&adopted_id);
        version.type_ = "MediaBrowser.Controller.Entities.Video".into();
        f.store.save_items(&[version]).await.unwrap();
        let path = "Film/Film - 4K-trailer.mkv";
        std::fs::write(f.media.join(path), b"").unwrap();
        if scoped {
            f.scanner
                .scan_paths(&[f.media.join(path).to_string_lossy().into_owned()])
                .await
                .unwrap();
        } else {
            f.scanner.scan_all().await.unwrap();
        }
        assert_eq!(f.row(path).await.owner_id, Some(adopted_id));
    }
}

#[derive(Default)]
struct SelectiveFs {
    fail: std::sync::Mutex<Option<String>>,
}

impl ferrofin_traits::filesystem::FileSystem for SelectiveFs {
    fn get_file_system_entries(&self, path: &str) -> Vec<ferrofin_model::io::FileSystemEntryInfo> {
        self.try_get_file_system_entries(path).unwrap_or_default()
    }
    fn try_get_file_system_entries(
        &self,
        path: &str,
    ) -> Result<Vec<ferrofin_model::io::FileSystemEntryInfo>, ferrofin_traits::error::ServiceError>
    {
        if self.fail.lock().unwrap().as_deref() == Some(path) {
            return Err(ferrofin_traits::error::ServiceError::NotFound(
                "injected listing failure".into(),
            ));
        }
        FerrofinFileSystem::new().try_get_file_system_entries(path)
    }
    fn get_drives(&self) -> Vec<ferrofin_model::io::FileSystemEntryInfo> {
        Vec::new()
    }
    fn file_exists(&self, path: &str) -> bool {
        FerrofinFileSystem::new().file_exists(path)
    }
    fn directory_exists(&self, path: &str) -> bool {
        FerrofinFileSystem::new().directory_exists(path)
    }
    fn validate_writable(&self, path: &str) -> Result<(), ferrofin_traits::error::ServiceError> {
        FerrofinFileSystem::new().validate_writable(path)
    }
    fn get_files(
        &self,
        path: &str,
        extensions: &[&str],
    ) -> Vec<ferrofin_traits::filesystem::FileMetadata> {
        FerrofinFileSystem::new().get_files(path, extensions)
    }
    fn read_file(&self, path: &str) -> Result<Vec<u8>, ferrofin_traits::error::ServiceError> {
        FerrofinFileSystem::new().read_file(path)
    }
}

impl Fixture {
    async fn legacy(&self, path: &str, owner: bool) -> String {
        let mut row = self.row(MOVIE).await;
        row.id = guid_to_db(Uuid::new_v4());
        row.path = Some(self.media.join(path).to_string_lossy().into_owned());
        row.type_ = "MediaBrowser.Controller.Entities.Video".into();
        row.is_movie = false;
        row.is_locked = true;
        row.extra_type = Some(7);
        row.owner_id = if owner {
            Some(self.row(MOVIE).await.id)
        } else {
            None
        };
        row.parent_id = Some(guid_to_db(self.library));
        row.top_parent_id = Some(guid_to_db(self.library));
        let id = row.id.clone();
        self.store.save_items(&[row]).await.unwrap();
        id
    }

    async fn has(&self, id: &str) -> bool {
        self.repo
            .retrieve_item(Uuid::parse_str(id).unwrap())
            .await
            .unwrap()
            .is_some()
    }
}

#[tokio::test]
async fn one_scan_removes_confirmed_legacy_exclusions_without_removing_files() {
    let excluded = [
        "Heat (1995)/._Heat.mkv",
        "Heat (1995)/Heat.sample.mkv",
        "Heat (1995)/@eaDir/stray.mkv",
        "Heat (1995)/.hidden/clip.mkv",
    ];
    for scoped in [false, true] {
        let mut files = vec![MOVIE, EXTRA, "Heat (1995)/Heat-sample.mkv"];
        files.extend(excluded);
        let f = Fixture::new(&files).await;
        f.scanner.scan_all().await.unwrap();
        let mut ids = Vec::new();
        for (i, path) in excluded.iter().enumerate() {
            ids.push(f.legacy(path, i % 2 == 0).await);
        }
        let good_id = f.row(EXTRA).await.id;
        if scoped {
            // One excluded exact path cannot remove siblings outside scope.
            let result = f
                .scanner
                .scan_paths(&[f.media.join(excluded[0]).to_string_lossy().into_owned()])
                .await
                .unwrap();
            assert_eq!(result.removed, 1);
            assert!(!f.has(&ids[0]).await);
            for id in &ids[1..] {
                assert!(f.has(id).await);
            }
            let result = f
                .scanner
                .scan_paths(&[f.media.join("Heat (1995)").to_string_lossy().into_owned()])
                .await
                .unwrap();
            assert_eq!(result.removed, 3);
        } else {
            assert_eq!(f.scanner.scan_all().await.unwrap().removed, 4);
        }
        for (path, id) in excluded.iter().zip(&ids) {
            assert!(!f.has(id).await);
            assert!(f.media.join(path).exists());
        }
        assert!(f.has(&good_id).await);
        assert_eq!(
            f.row("Heat (1995)/Heat-sample.mkv").await.extra_type,
            Some(7)
        );
        assert_eq!(f.scanner.scan_all().await.unwrap().removed, 0);
    }
}

#[tokio::test]
async fn extras_in_ineligible_folders_are_cleaned_without_deleting_movies() {
    let f = Fixture::new(&[MOVIE, EXTRA, "Heat (1995)/Release/Other.mkv"]).await;
    f.scanner.scan_all().await.unwrap();
    let id = f.legacy(EXTRA, false).await;
    assert_eq!(f.scanner.scan_all().await.unwrap().removed, 1);
    assert!(!f.has(&id).await);
    assert!(f.media.join(EXTRA).exists());
    f.assert_browse(2).await;
}

#[tokio::test]
async fn exclusions_are_not_pruned_after_failed_listing_missing_mount_or_cancellation() {
    use ferrofin_core::library_scan::{ScanCancel, ScanRun};
    use ferrofin_traits::library::ScanTarget;
    use ferrofin_traits::providers::MetadataRefreshOptions;
    let ignored = "Heat (1995)/._Heat.mkv";
    let f = Fixture::new(&[MOVIE, ignored]).await;
    f.scanner.scan_all().await.unwrap();
    let id = f.legacy(ignored, false).await;
    *f.fs.fail.lock().unwrap() = Some(f.media.join("Heat (1995)").to_string_lossy().into_owned());
    assert_eq!(f.scanner.scan_all().await.unwrap().removed, 0);
    assert!(f.has(&id).await);
    *f.fs.fail.lock().unwrap() = None;
    let parked = f.tmp.path().join("parked");
    std::fs::rename(&f.media, &parked).unwrap();
    assert_eq!(f.scanner.scan_all().await.unwrap().removed, 0);
    std::fs::create_dir(&f.media).unwrap();
    assert_eq!(f.scanner.scan_all().await.unwrap().removed, 0);
    assert!(f.has(&id).await);
    std::fs::remove_dir(&f.media).unwrap();
    std::fs::rename(&parked, &f.media).unwrap();
    let cancel = ScanCancel::new();
    let at_end = {
        let cancel = cancel.clone();
        move |pct: f64| {
            if pct >= 96.0 {
                cancel.cancel();
            }
        }
    };
    let options = MetadataRefreshOptions::default();
    let result = f
        .scanner
        .scan_target(
            &ScanTarget::All,
            ScanRun::new(&options, &options, &cancel).with_progress(&at_end),
        )
        .await
        .unwrap();
    assert!(result.stopped);
    assert_eq!(result.removed, 0);
    assert!(f.has(&id).await);
    assert_eq!(f.scanner.scan_all().await.unwrap().removed, 1);
}

async fn play(db: &Database, item: &str) {
    let user = "0000000A-0000-0000-0000-00000000000A";
    sqlx::query(
        r#"INSERT OR IGNORE INTO "Users"
           ("Id", "AuthenticationProviderId", "DisplayCollectionsView",
            "DisplayMissingEpisodes", "EnableAutoLogin", "EnableLocalPassword",
            "EnableNextEpisodeAutoPlay", "EnableUserPreferenceAccess",
            "HidePlayedInLatest", "InternalId", "InvalidLoginAttemptCount",
            "MaxActiveSessions", "MustUpdatePassword",
            "PasswordResetProviderId", "PlayDefaultAudioTrack",
            "RememberAudioSelections", "RememberSubtitleSelections",
            "RowVersion", "SubtitleMode", "SyncPlayAccess", "Username", "NormalizedUsername")
           VALUES (?1, '', 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, '', 1, 1, 1, 0, 0, 0, 'u', 'U')"#,
    )
    .bind(user)
    .execute(db.writer())
    .await
    .expect("user");
    sqlx::query(
        r#"INSERT INTO "UserData"
           ("ItemId", "UserId", "CustomDataKey", "IsFavorite", "PlayCount",
            "PlaybackPositionTicks", "Played")
           VALUES (?1, ?2, ?1, 0, 1, 0, 1)"#,
    )
    .bind(item)
    .bind(user)
    .execute(db.writer())
    .await
    .expect("user data");
}
