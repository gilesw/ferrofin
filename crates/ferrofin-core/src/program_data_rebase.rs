//! Rebases the absolute paths a database stores under a previous program-data
//! directory onto this server's directories.
//!
//! Jellyfin stores `{program data}/root`, its library folders and every image
//! path absolute; only `{program data}/data` and the metadata directory are
//! written as `%AppDataPath%`/`%MetadataPath%` tokens. The ids of items under
//! program data are derived from the path relative to it (`GetNewItemId`), so
//! after the directory moves (a Debian `/var/lib/jellyfin` adopted into
//! `/var/lib/ferrofin`, or Ferrofin's own data directory relocated) every id
//! still matches and only the stored paths are stale. The `AggregateFolder`
//! row, looked up by that id, still carries the old `{program data}/root`,
//! which is how the old directory is found.

use ferrofin_db::Database;
use ferrofin_db::store::guid_to_db;
use ferrofin_traits::error::ServiceError;
use uuid::Uuid;

use crate::db_error::db_err;

/// The columns that can hold an absolute program-data path.
const PATH_COLUMNS: [(&str, &str); 5] = [
    ("BaseItems", "Path"),
    ("BaseItemImageInfos", "Path"),
    ("Chapters", "ImagePath"),
    ("MediaStreamInfos", "Path"),
    ("ImageInfos", "Path"),
];

/// This server's directories, as absolute paths.
#[derive(Debug, Clone)]
pub struct ProgramDataDirs {
    /// `{program data}/root`, the aggregate root folder.
    pub root: String,
    /// The internal metadata directory.
    pub metadata: String,
    /// `{program data}/data`.
    pub data: String,
    /// The configuration directory.
    pub config: String,
}

/// What [`rebase_program_data_paths`] changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rebased {
    /// The previous program-data directory.
    pub from: String,
    /// Rows rewritten across every path column.
    pub rows: u64,
}

/// Rewrites paths under the program-data directory recorded on the
/// `AggregateFolder` row `aggregate_id` onto `dirs`.
///
/// `{old}/root`, `{old}/metadata`, `{old}/data` and `{old}/config` map to the
/// matching directory in `dirs`, matching the directory itself or a path below
/// it. A profile image stored under a configuration directory outside program
/// data (Debian's `/etc/jellyfin/users/<name>/`) moves to `{config}/users/<name>/`.
/// Tokenised paths are left alone. Returns `None` when the aggregate row is
/// missing or already at `dirs.root`, which makes every later boot a single
/// row read.
///
/// # Errors
/// Returns [`ServiceError`] if a query fails; no row is changed in that case.
pub async fn rebase_program_data_paths(
    db: &Database,
    aggregate_id: Uuid,
    dirs: &ProgramDataDirs,
) -> Result<Option<Rebased>, ServiceError> {
    let stored: Option<Option<String>> =
        sqlx::query_scalar(r#"SELECT "Path" FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(guid_to_db(aggregate_id))
            .fetch_optional(db.writer())
            .await
            .map_err(db_err)?;
    let root = trim_separator(&dirs.root);
    let Some(Some(stored)) = stored else {
        return Ok(None);
    };
    let stored = trim_separator(&stored);
    if stored == root {
        return Ok(None);
    }
    let Some(old) = stored.strip_suffix("/root").filter(|old| !old.is_empty()) else {
        tracing::warn!(
            path = stored,
            "the aggregate root folder path does not end in /root; stored paths were not rebased"
        );
        return Ok(None);
    };

    let mappings = [
        (format!("{old}/root"), root.to_owned()),
        (
            format!("{old}/metadata"),
            trim_separator(&dirs.metadata).to_owned(),
        ),
        (format!("{old}/data"), trim_separator(&dirs.data).to_owned()),
        (
            format!("{old}/config"),
            trim_separator(&dirs.config).to_owned(),
        ),
    ];
    let mut tx = db.writer().begin().await.map_err(db_err)?;
    let mut rows = 0;
    for (from, to) in mappings.iter().filter(|(from, to)| from != to) {
        for (table, column) in PATH_COLUMNS {
            // Table and column names come from `PATH_COLUMNS`; both paths are bound.
            rows += sqlx::query(sqlx::AssertSqlSafe(format!(
                r#"UPDATE "{table}" SET "{column}" = ?2 || substr("{column}", length(?1) + 1)
                   WHERE "{column}" = ?1 OR substr("{column}", 1, length(?1) + 1) = ?1 || '/'"#
            )))
            .bind(from)
            .bind(to)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?
            .rows_affected();
        }
    }
    rows += rebase_profile_images(&mut tx, trim_separator(&dirs.config)).await?;
    tx.commit().await.map_err(db_err)?;
    Ok(Some(Rebased {
        from: old.to_owned(),
        rows,
    }))
}

/// Moves `…/users/<Username>/<file>` profile images that are not already under
/// `config` to `{config}/users/<Username>/<file>`.
async fn rebase_profile_images(
    tx: &mut sqlx::SqliteConnection,
    config: &str,
) -> Result<u64, ServiceError> {
    let images: Vec<(i64, String, String)> = sqlx::query_as(
        r#"SELECT i."Id", i."Path", u."Username"
           FROM "ImageInfos" i JOIN "Users" u ON u."Id" = i."UserId""#,
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)?;
    let users_dir = format!("{config}/users/");
    let mut rows = 0;
    for (id, path, username) in images {
        if path.starts_with(&users_dir) {
            continue;
        }
        let marker = format!("/users/{username}/");
        let Some(file) = path
            .rfind(&marker)
            .map(|at| &path[at + marker.len()..])
            .filter(|file| !file.is_empty() && !file.contains('/'))
        else {
            continue;
        };
        rows += sqlx::query(r#"UPDATE "ImageInfos" SET "Path" = ?2 WHERE "Id" = ?1"#)
            .bind(id)
            .bind(format!("{users_dir}{username}/{file}"))
            .execute(&mut *tx)
            .await
            .map_err(db_err)?
            .rows_affected();
    }
    Ok(rows)
}

fn trim_separator(path: &str) -> &str {
    match path.trim_end_matches('/') {
        "" => path,
        trimmed => trimmed,
    }
}

#[cfg(test)]
mod tests {
    use ferrofin_model::data::BaseItemKind;

    use super::*;
    use crate::test_support::{seed_folder_item, seed_named_user, test_db};

    const AGGREGATE: Uuid = Uuid::from_u128(0xa1);
    const LIBRARY: Uuid = Uuid::from_u128(0xa2);
    const MOVIE: Uuid = Uuid::from_u128(0xa3);
    const PLAYLISTS: Uuid = Uuid::from_u128(0xa4);
    const USER: Uuid = Uuid::from_u128(0xa5);

    fn dirs() -> ProgramDataDirs {
        ProgramDataDirs {
            root: "/var/lib/ferrofin/root".to_owned(),
            metadata: "/var/lib/ferrofin/metadata".to_owned(),
            data: "/var/lib/ferrofin/data".to_owned(),
            config: "/var/lib/ferrofin/config/".to_owned(),
        }
    }

    async fn seed(db: &Database, id: Uuid, kind: BaseItemKind, path: &str) {
        seed_folder_item(db, id, kind, "x", None).await;
        sqlx::query(r#"UPDATE "BaseItems" SET "Path" = ?2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(id))
            .bind(path)
            .execute(db.writer())
            .await
            .unwrap();
    }

    async fn seed_image(db: &Database, item: Uuid, path: &str) {
        sqlx::query(
            r#"INSERT INTO "BaseItemImageInfos"
               ("Id", "ItemId", "Path", "ImageType", "DateModified", "Width", "Height")
               VALUES (?1, ?2, ?3, 0, '2026-01-01 00:00:00', 0, 0)"#,
        )
        .bind(guid_to_db(Uuid::new_v4()))
        .bind(guid_to_db(item))
        .bind(path)
        .execute(db.writer())
        .await
        .unwrap();
    }

    async fn item_path(db: &Database, id: Uuid) -> String {
        sqlx::query_scalar(r#"SELECT "Path" FROM "BaseItems" WHERE "Id" = ?1"#)
            .bind(guid_to_db(id))
            .fetch_one(db.writer())
            .await
            .unwrap()
    }

    async fn image_paths(db: &Database) -> Vec<String> {
        sqlx::query_scalar(r#"SELECT "Path" FROM "BaseItemImageInfos" ORDER BY "Path""#)
            .fetch_all(db.writer())
            .await
            .unwrap()
    }

    async fn debian_adoption() -> Database {
        let db = test_db().await;
        seed(
            &db,
            AGGREGATE,
            BaseItemKind::AggregateFolder,
            "/var/lib/jellyfin/root",
        )
        .await;
        seed(
            &db,
            LIBRARY,
            BaseItemKind::CollectionFolder,
            "/var/lib/jellyfin/root/default/Movies",
        )
        .await;
        seed(
            &db,
            MOVIE,
            BaseItemKind::Movie,
            "/srv/media/movies/Honeyland (2019)",
        )
        .await;
        seed(
            &db,
            PLAYLISTS,
            BaseItemKind::PlaylistsFolder,
            "%AppDataPath%/playlists",
        )
        .await;
        seed_image(
            &db,
            MOVIE,
            "/var/lib/jellyfin/metadata/library/cf/cf75/poster.jpg",
        )
        .await;
        seed_image(&db, MOVIE, "/srv/media/movies/Honeyland (2019)/fanart.jpg").await;
        seed_image(&db, MOVIE, "/var/lib/jellyfin/metadata-other/poster.jpg").await;
        db
    }

    #[tokio::test]
    async fn rebases_root_library_and_image_paths_onto_this_server() {
        let db = debian_adoption().await;

        let rebased = rebase_program_data_paths(&db, AGGREGATE, &dirs())
            .await
            .unwrap();

        assert_eq!(
            rebased,
            Some(Rebased {
                from: "/var/lib/jellyfin".to_owned(),
                rows: 3,
            })
        );
        assert_eq!(item_path(&db, AGGREGATE).await, "/var/lib/ferrofin/root");
        assert_eq!(
            item_path(&db, LIBRARY).await,
            "/var/lib/ferrofin/root/default/Movies"
        );
        assert_eq!(
            item_path(&db, MOVIE).await,
            "/srv/media/movies/Honeyland (2019)"
        );
        assert_eq!(item_path(&db, PLAYLISTS).await, "%AppDataPath%/playlists");
        assert_eq!(
            image_paths(&db).await,
            [
                "/srv/media/movies/Honeyland (2019)/fanart.jpg",
                "/var/lib/ferrofin/metadata/library/cf/cf75/poster.jpg",
                "/var/lib/jellyfin/metadata-other/poster.jpg",
            ]
        );
    }

    #[tokio::test]
    async fn a_second_boot_is_a_no_op() {
        let db = debian_adoption().await;
        rebase_program_data_paths(&db, AGGREGATE, &dirs())
            .await
            .unwrap();

        assert_eq!(
            rebase_program_data_paths(&db, AGGREGATE, &dirs())
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn a_database_without_the_aggregate_row_is_left_alone() {
        let db = test_db().await;
        seed(
            &db,
            MOVIE,
            BaseItemKind::Movie,
            "/srv/media/movies/Honeyland (2019)",
        )
        .await;
        seed_image(&db, MOVIE, "/var/lib/jellyfin/metadata/library/poster.jpg").await;

        assert_eq!(
            rebase_program_data_paths(&db, AGGREGATE, &dirs())
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            image_paths(&db).await,
            ["/var/lib/jellyfin/metadata/library/poster.jpg"]
        );
    }

    #[tokio::test]
    async fn an_aggregate_path_not_ending_in_root_is_left_alone() {
        let db = test_db().await;
        seed(
            &db,
            AGGREGATE,
            BaseItemKind::AggregateFolder,
            "/srv/library",
        )
        .await;

        assert_eq!(
            rebase_program_data_paths(&db, AGGREGATE, &dirs())
                .await
                .unwrap(),
            None
        );
        assert_eq!(item_path(&db, AGGREGATE).await, "/srv/library");
    }

    #[tokio::test]
    async fn a_relocated_ferrofin_data_directory_is_rebased() {
        let db = test_db().await;
        seed(
            &db,
            AGGREGATE,
            BaseItemKind::AggregateFolder,
            "/var/lib/ferrofin/data/root",
        )
        .await;
        seed(
            &db,
            MOVIE,
            BaseItemKind::Movie,
            "/srv/media/movies/Honeyland (2019)",
        )
        .await;
        seed_image(
            &db,
            MOVIE,
            "/var/lib/ferrofin/data/metadata/library/poster.jpg",
        )
        .await;

        let rebased = rebase_program_data_paths(&db, AGGREGATE, &dirs())
            .await
            .unwrap();

        assert_eq!(
            rebased.map(|r| r.from),
            Some("/var/lib/ferrofin/data".to_owned())
        );
        assert_eq!(item_path(&db, AGGREGATE).await, "/var/lib/ferrofin/root");
        assert_eq!(
            image_paths(&db).await,
            ["/var/lib/ferrofin/metadata/library/poster.jpg"]
        );
    }

    #[tokio::test]
    async fn profile_images_move_into_the_config_users_directory() {
        let db = debian_adoption().await;
        seed_named_user(&db, USER, "giles").await;
        sqlx::query(
            r#"INSERT INTO "ImageInfos" ("LastModified", "Path", "UserId") VALUES ('2026-01-01', ?1, ?2)"#,
        )
        .bind("/etc/jellyfin/users/giles/profile.png")
        .bind(guid_to_db(USER))
        .execute(db.writer())
        .await
        .unwrap();

        rebase_program_data_paths(&db, AGGREGATE, &dirs())
            .await
            .unwrap();

        let path: String = sqlx::query_scalar(r#"SELECT "Path" FROM "ImageInfos""#)
            .fetch_one(db.writer())
            .await
            .unwrap();
        assert_eq!(path, "/var/lib/ferrofin/config/users/giles/profile.png");
    }
}
