mod db;
mod queries;
mod schema;

pub use db::Store;
pub use queries::FolderRow;

use crate::error::Result;
use crate::types::{Envelope, Flags};

impl Store {
    pub async fn upsert_account(&self, email: String, display_name: Option<String>) -> Result<i64> {
        self.run(move |conn| queries::upsert_account(conn, &email, display_name.as_deref()))
            .await
    }

    pub async fn get_or_create_folder(&self, account_id: i64, name: String) -> Result<FolderRow> {
        self.run(move |conn| queries::get_or_create_folder(conn, account_id, &name))
            .await
    }

    pub async fn update_folder_uid_state(
        &self,
        folder_id: i64,
        uidvalidity: u32,
        uidnext: u32,
    ) -> Result<()> {
        self.run(move |conn| queries::update_folder_uid_state(conn, folder_id, uidvalidity, uidnext))
            .await
    }

    pub async fn clear_folder_messages(&self, folder_id: i64) -> Result<()> {
        self.run(move |conn| queries::clear_folder_messages(conn, folder_id))
            .await
    }

    pub async fn upsert_envelopes(&self, folder_id: i64, envelopes: Vec<Envelope>) -> Result<()> {
        self.run(move |conn| {
            for env in &envelopes {
                queries::upsert_envelope(conn, folder_id, env)?;
            }
            Ok(())
        })
        .await
    }

    pub async fn update_message_flags(&self, folder_id: i64, uid: u32, flags: Flags) -> Result<()> {
        self.run(move |conn| queries::update_message_flags(conn, folder_id, uid, flags))
            .await
    }

    pub async fn list_envelopes(&self, folder_id: i64, limit: u32) -> Result<Vec<Envelope>> {
        self.run(move |conn| queries::list_envelopes(conn, folder_id, limit))
            .await
    }

    pub async fn max_uid(&self, folder_id: i64) -> Result<Option<u32>> {
        self.run(move |conn| queries::max_uid(conn, folder_id)).await
    }

    pub async fn recent_uids(&self, folder_id: i64, limit: u32) -> Result<Vec<u32>> {
        self.run(move |conn| queries::recent_uids(conn, folder_id, limit))
            .await
    }

    pub async fn set_message_raw_path(&self, folder_id: i64, uid: u32, path: String) -> Result<()> {
        self.run(move |conn| queries::set_message_raw_path(conn, folder_id, uid, &path))
            .await
    }

    pub async fn get_message_raw_path(&self, folder_id: i64, uid: u32) -> Result<Option<String>> {
        self.run(move |conn| queries::get_message_raw_path(conn, folder_id, uid))
            .await
    }
}
