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
    _tmp: tempfile::TempDir,
    media: PathBuf,
    db: Database,
    repo: Arc<FerrofinItemRepository>,
    store: Arc<FerrofinItemPersistenceService>,
    scanner: LibraryScanner,
    library: Uuid,
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
        let scanner = LibraryScanner::new(vf, Arc::new(FerrofinFileSystem::new()), store.clone())
            .with_items(repo.clone());
        Self {
            _tmp: tmp,
            media,
            db,
            repo,
            store,
            scanner,
            library,
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
    f.scanner.scan_all().await.unwrap();
    f.assert_browse(1).await;
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
