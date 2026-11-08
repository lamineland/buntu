use crate::db::{DatabaseManager, DbError};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

fn new_profile_uuid(name: &str, now: i64) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let digest = blake3::hash(format!("{name}|{nanos}|{seq}").as_bytes());
    let short: String = digest.to_hex().chars().take(12).collect();
    format!("prof_{now}_{short}")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
pub struct ProfileRecord {
    pub id: i64,
    pub uuid: String,
    pub name: String,
    pub avatar: String,
    pub is_kids: bool,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
pub struct FavoriteFolderRecord {
    pub id: i64,
    pub profile_id: i64,
    pub name: String,
    pub position: i32,
    pub created_at: i64,
    pub item_count: u32,
}

fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn normalize_group_key(group: &str) -> String {
    group.trim().to_lowercase()
}

pub fn is_adult_category(group: &str) -> bool {
    let lower = group.trim().to_lowercase();
    let keywords = [
        "adult", "adults", "adulte", "adultes", "xxx", "+18", "18+", "porn", "porno",
        "erotic", "erotique", "nsfw", "for adults", "pour adultes",
    ];
    keywords.iter().any(|&kw| {
        if kw == "+18" || kw == "18+" || kw == "xxx" {
            lower.contains(kw)
        } else {
            // Check word boundaries
            lower.split(|c: char| !c.is_alphanumeric()).any(|w| w == kw)
        }
    })
}

impl DatabaseManager {
    // MARK: - Profiles

    pub fn create_profile(&self, name: &str, avatar: &str, is_kids: bool) -> Result<ProfileRecord, DbError> {
        let now = now_epoch_secs();
        let uuid = new_profile_uuid(name, now);
        let is_kids_val = if is_kids { 1 } else { 0 };

        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO profiles(uuid, name, avatar, is_kids, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                params![uuid, name, avatar, is_kids_val, now],
            )?;
            let id = conn.last_insert_rowid();
            Ok(ProfileRecord {
                id,
                uuid,
                name: name.to_string(),
                avatar: avatar.to_string(),
                is_kids,
                created_at: now,
            })
        })
    }

    pub fn list_profiles(&self) -> Result<Vec<ProfileRecord>, DbError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT id, uuid, name, avatar, is_kids, created_at FROM profiles ORDER BY id ASC")?;
            let rows = stmt.query_map([], |row| {
                Ok(ProfileRecord {
                    id: row.get(0)?,
                    uuid: row.get(1)?,
                    name: row.get(2)?,
                    avatar: row.get(3)?,
                    is_kids: row.get::<_, i32>(4)? != 0,
                    created_at: row.get(5)?,
                })
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    pub fn get_profile(&self, id: i64) -> Result<Option<ProfileRecord>, DbError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT id, uuid, name, avatar, is_kids, created_at FROM profiles WHERE id = ?1")?;
            let mut rows = stmt.query_map(params![id], |row| {
                Ok(ProfileRecord {
                    id: row.get(0)?,
                    uuid: row.get(1)?,
                    name: row.get(2)?,
                    avatar: row.get(3)?,
                    is_kids: row.get::<_, i32>(4)? != 0,
                    created_at: row.get(5)?,
                })
            })?;
            if let Some(r) = rows.next() {
                Ok(Some(r?))
            } else {
                Ok(None)
            }
        })
    }

    pub fn update_profile(&self, id: i64, name: &str, avatar: &str, is_kids: bool) -> Result<(), DbError> {
        let is_kids_val = if is_kids { 1 } else { 0 };
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE profiles SET name = ?1, avatar = ?2, is_kids = ?3, updated_at = ?5 WHERE id = ?4",
                params![name, avatar, is_kids_val, id, now_epoch_secs()],
            )?;
            Ok(())
        })?;
        self.refresh_kids_flag()
    }

    // MARK: - Kids allow-list

    pub fn allow_group(&self, profile_id: i64, kind: &str, group: &str) -> Result<(), DbError> {
        let key = normalize_group_key(group);
        // A Kids profile can be given a whole theme (`theme:live` / `kids`), never the adult one (PRD-33).
        if kind.starts_with("theme:") && key == "adult" {
            return Err(DbError::Query("The adult theme can never be allowed for a Kids profile".to_string()));
        }
        self.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO kids_allowed_groups(profile_id, kind, group_key) VALUES (?1, ?2, ?3)",
                params![profile_id, kind, key],
            )?;
            Ok(())
        })
    }

    pub fn disallow_group(&self, profile_id: i64, kind: &str, group: &str) -> Result<bool, DbError> {
        let key = normalize_group_key(group);
        self.with_conn(|conn| {
            let count = conn.execute(
                "DELETE FROM kids_allowed_groups WHERE profile_id = ?1 AND kind = ?2 AND group_key = ?3",
                params![profile_id, kind, key],
            )?;
            Ok(count > 0)
        })
    }

    pub fn list_allowed_groups(&self, profile_id: i64, kind: &str) -> Result<Vec<String>, DbError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT group_key FROM kids_allowed_groups WHERE profile_id = ?1 AND kind = ?2 ORDER BY group_key ASC",
            )?;
            let rows = stmt.query_map(params![profile_id, kind], |row| row.get(0))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    pub fn delete_profile(&self, id: i64) -> Result<bool, DbError> {
        if id == 1 {
            return Err(DbError::Query("Cannot delete default profile 1".to_string()));
        }
        self.with_conn(|conn| {
            let uuid: Option<String> = conn.query_row("SELECT uuid FROM profiles WHERE id = ?1", params![id], |r| r.get(0)).ok();
            let deleted = conn.execute("DELETE FROM profiles WHERE id = ?1", params![id])?;
            if deleted > 0 {
                if let Some(uuid) = &uuid {
                    // Other devices learn about it through the tombstone.
                    super::sync_repo::upsert_tombstone(conn, super::sync_repo::TOMBSTONE_PROFILE, uuid, now_epoch_secs())?;
                }
                let _ = conn.execute("DELETE FROM favorites WHERE profile_id = ?1", params![id]);
                let _ = conn.execute("DELETE FROM watch_history WHERE profile_id = ?1", params![id]);
                let _ = conn.execute("DELETE FROM hidden_groups WHERE profile_id = ?1", params![id]);
                let _ = conn.execute("DELETE FROM kids_allowed_groups WHERE profile_id = ?1", params![id]);
                let _ = conn.execute("DELETE FROM hidden_items WHERE profile_id = ?1", params![id]);
                let _ = conn.execute("DELETE FROM custom_order WHERE profile_id = ?1", params![id]);
                let _ = conn.execute("DELETE FROM favorite_folders WHERE profile_id = ?1", params![id]);
            }
            Ok(deleted > 0)
        })
    }

    // MARK: - Hidden groups (Bouquet hiding)

    pub fn hide_group(&self, profile_id: i64, kind: &str, group: &str) -> Result<(), DbError> {
        let key = normalize_group_key(group);
        self.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO hidden_groups(profile_id, kind, group_key) VALUES (?1, ?2, ?3)",
                params![profile_id, kind, key],
            )?;
            Ok(())
        })
    }

    pub fn unhide_group(&self, profile_id: i64, kind: &str, group: &str) -> Result<bool, DbError> {
        let key = normalize_group_key(group);
        self.with_conn(|conn| {
            let count = conn.execute(
                "DELETE FROM hidden_groups WHERE profile_id = ?1 AND kind = ?2 AND group_key = ?3",
                params![profile_id, kind, key],
            )?;
            Ok(count > 0)
        })
    }

    pub fn list_hidden_groups(&self, profile_id: i64, kind: &str) -> Result<Vec<String>, DbError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT group_key FROM hidden_groups WHERE profile_id = ?1 AND kind = ?2 ORDER BY group_key ASC")?;
            let rows = stmt.query_map(params![profile_id, kind], |row| row.get(0))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    // MARK: - Hidden items

    pub fn hide_item(&self, profile_id: i64, stable_hash: &str) -> Result<(), DbError> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO hidden_items(profile_id, stable_hash) VALUES (?1, ?2)",
                params![profile_id, stable_hash],
            )?;
            Ok(())
        })
    }

    pub fn unhide_item(&self, profile_id: i64, stable_hash: &str) -> Result<bool, DbError> {
        self.with_conn(|conn| {
            let count = conn.execute(
                "DELETE FROM hidden_items WHERE profile_id = ?1 AND stable_hash = ?2",
                params![profile_id, stable_hash],
            )?;
            Ok(count > 0)
        })
    }

    pub fn list_hidden_items(&self, profile_id: i64) -> Result<Vec<String>, DbError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT stable_hash FROM hidden_items WHERE profile_id = ?1")?;
            let rows = stmt.query_map(params![profile_id], |row| row.get(0))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    // MARK: - Custom Ordering

    pub fn set_custom_order(&self, profile_id: i64, scope: &str, key: &str, position: i32) -> Result<(), DbError> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO custom_order(profile_id, scope, key, position) VALUES (?1, ?2, ?3, ?4)",
                params![profile_id, scope, key, position],
            )?;
            Ok(())
        })
    }

    pub fn get_custom_order(&self, profile_id: i64, scope: &str) -> Result<Vec<(String, i32)>, DbError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT key, position FROM custom_order WHERE profile_id = ?1 AND scope = ?2 ORDER BY position ASC")?;
            let rows = stmt.query_map(params![profile_id, scope], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    // MARK: - Favorite Folders

    pub fn create_favorite_folder(&self, profile_id: i64, name: &str, position: i32) -> Result<i64, DbError> {
        let now = now_epoch_secs();
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO favorite_folders(profile_id, name, position, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![profile_id, name, position, now],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    pub fn delete_favorite_folder(&self, id: i64) -> Result<bool, DbError> {
        self.with_conn(|conn| {
            let count = conn.execute("DELETE FROM favorite_folders WHERE id = ?1", params![id])?;
            Ok(count > 0)
        })
    }

    pub fn list_favorite_folders(&self, profile_id: i64) -> Result<Vec<FavoriteFolderRecord>, DbError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT f.id, f.profile_id, f.name, f.position, f.created_at,
                        (SELECT COUNT(*) FROM favorite_folder_items i WHERE i.folder_id = f.id)
                 FROM favorite_folders f
                 WHERE f.profile_id = ?1
                 ORDER BY f.position ASC, f.id ASC",
            )?;
            let rows = stmt.query_map(params![profile_id], |row| {
                Ok(FavoriteFolderRecord {
                    id: row.get(0)?,
                    profile_id: row.get(1)?,
                    name: row.get(2)?,
                    position: row.get(3)?,
                    created_at: row.get(4)?,
                    item_count: row.get::<_, i64>(5)? as u32,
                })
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    pub fn add_to_favorite_folder(&self, folder_id: i64, stable_hash: &str, position: i32) -> Result<(), DbError> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO favorite_folder_items(folder_id, stable_hash, position) VALUES (?1, ?2, ?3)",
                params![folder_id, stable_hash, position],
            )?;
            Ok(())
        })
    }

    pub fn remove_from_favorite_folder(&self, folder_id: i64, stable_hash: &str) -> Result<bool, DbError> {
        self.with_conn(|conn| {
            let count = conn.execute(
                "DELETE FROM favorite_folder_items WHERE folder_id = ?1 AND stable_hash = ?2",
                params![folder_id, stable_hash],
            )?;
            Ok(count > 0)
        })
    }

    pub fn list_favorite_folder_items(&self, folder_id: i64) -> Result<Vec<String>, DbError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT stable_hash FROM favorite_folder_items WHERE folder_id = ?1 ORDER BY position ASC")?;
            let rows = stmt.query_map(params![folder_id], |row| row.get(0))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_adult_category_detection() {
        assert!(is_adult_category("XXX Adultes"));
        assert!(is_adult_category("FR: +18"));
        assert!(is_adult_category("Porn Stars"));
        assert!(is_adult_category("Section Erotique"));
        assert!(!is_adult_category("France 2 HD"));
        assert!(!is_adult_category("Jeunesse & Enfants"));
        assert!(!is_adult_category("Documentaires"));
    }

    #[test]
    fn test_profiles_crud() {
        let db = DatabaseManager::open_in_memory().expect("db");
        let initial = db.list_profiles().expect("list");
        assert_eq!(initial.len(), 1);
        assert_eq!(initial[0].name, "Principal");

        let p2 = db.create_profile("Kids", "figure.child", true).expect("create");
        assert_eq!(p2.name, "Kids");
        assert!(p2.is_kids);

        let list = db.list_profiles().expect("list");
        assert_eq!(list.len(), 2);

        db.update_profile(p2.id, "Kids Room", "figure.child", true).expect("update");
        let updated = db.get_profile(p2.id).expect("get").expect("found");
        assert_eq!(updated.name, "Kids Room");

        // Delete profile 1 forbidden
        assert!(db.delete_profile(1).is_err());

        // Delete profile 2 succeeds
        assert!(db.delete_profile(p2.id).expect("delete"));
        assert_eq!(db.list_profiles().expect("list").len(), 1);
    }

    #[test]
    fn test_bouquets_and_custom_order() {
        let db = DatabaseManager::open_in_memory().expect("db");

        // Hide and unhide group
        db.hide_group(1, "live", "Sport").expect("hide");
        let hidden = db.list_hidden_groups(1, "live").expect("list");
        assert_eq!(hidden, vec!["sport"]);

        db.unhide_group(1, "live", "Sport").expect("unhide");
        let hidden2 = db.list_hidden_groups(1, "live").expect("list");
        assert!(hidden2.is_empty());

        // Custom order
        db.set_custom_order(1, "channel", "tf1_hash", 1).expect("order");
        db.set_custom_order(1, "channel", "fr2_hash", 2).expect("order");
        let order = db.get_custom_order(1, "channel").expect("get");
        assert_eq!(order.len(), 2);
        assert_eq!(order[0], ("tf1_hash".to_string(), 1));

        // Favorite folders
        let f_id = db.create_favorite_folder(1, "Football", 0).expect("folder");
        db.add_to_favorite_folder(f_id, "canal_foot", 1).expect("add");
        let folders = db.list_favorite_folders(1).expect("list");
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].item_count, 1);

        let items = db.list_favorite_folder_items(f_id).expect("items");
        assert_eq!(items, vec!["canal_foot"]);

        db.remove_from_favorite_folder(f_id, "canal_foot").expect("remove");
        assert!(db.list_favorite_folder_items(f_id).expect("items").is_empty());
    }
}
