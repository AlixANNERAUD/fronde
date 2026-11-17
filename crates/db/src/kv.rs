//! Small values by scope and key, each optionally expiring. An expired value reads as missing
//! at once; [`purge_expired`] removes the rows later.

use chrono::Utc;
use fronde_entity::kv;
use sea_orm::sea_query::OnConflict;
use sea_orm::{ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, Set};
use std::time::Duration;

/// The value under `scope` and `key`, unless it is missing or expired.
pub async fn get<C: ConnectionTrait>(
    db: &C,
    scope: &str,
    key: &str,
) -> Result<Option<Vec<u8>>, DbErr> {
    let row = kv::Entity::find_by_id((scope.to_owned(), key.to_owned()))
        .one(db)
        .await?;
    let now = Utc::now();
    Ok(row
        .filter(|row| row.expires_at.is_none_or(|at| at > now))
        .map(|row| row.value))
}

/// Store `value` under `scope` and `key`, replacing any earlier value. With a `ttl`, the value
/// expires that long from now; without, it never does.
pub async fn set<C: ConnectionTrait>(
    db: &C,
    scope: &str,
    key: &str,
    value: Vec<u8>,
    ttl: Option<Duration>,
) -> Result<(), DbErr> {
    let expires_at = ttl
        .map(|ttl| {
            chrono::Duration::from_std(ttl)
                .ok()
                .and_then(|ttl| Utc::now().checked_add_signed(ttl))
                .ok_or_else(|| DbErr::Custom(format!("ttl {ttl:?} is too long")))
        })
        .transpose()?;
    let row = kv::ActiveModel {
        scope: Set(scope.to_owned()),
        key: Set(key.to_owned()),
        value: Set(value),
        expires_at: Set(expires_at),
    };
    kv::Entity::insert(row)
        .on_conflict(
            OnConflict::columns([kv::Column::Scope, kv::Column::Key])
                .update_columns([kv::Column::Value, kv::Column::ExpiresAt])
                .to_owned(),
        )
        .exec(db)
        .await?;
    Ok(())
}

/// Remove the value under `scope` and `key`; true if there was one, expired or not.
pub async fn delete<C: ConnectionTrait>(db: &C, scope: &str, key: &str) -> Result<bool, DbErr> {
    let result = kv::Entity::delete_by_id((scope.to_owned(), key.to_owned()))
        .exec(db)
        .await?;
    Ok(result.rows_affected > 0)
}

/// Remove every expired row; returns how many.
pub async fn purge_expired<C: ConnectionTrait>(db: &C) -> Result<u64, DbErr> {
    let result = kv::Entity::delete_many()
        .filter(kv::Column::ExpiresAt.lte(Utc::now()))
        .exec(db)
        .await?;
    Ok(result.rows_affected)
}
