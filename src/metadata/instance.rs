//! Stable non-secret identity for one LFS Cloud metadata database.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::rand::{SecureRandom, SystemRandom};

use crate::{ServerError, ServerResult};

use super::MetadataDatabase;

const INSTANCE_ID_KEY: &str = "instance_id";

impl MetadataDatabase {
    /// Loads or creates the stable identity used to namespace native secrets.
    ///
    /// The identifier is deliberately stored in SQLite rather than derived
    /// from its filesystem path, so moving the configuration and metadata
    /// files does not orphan the corresponding credential-store entry.
    pub(crate) fn instance_id(&self) -> ServerResult<String> {
        if let Some(value) = self.load_property(INSTANCE_ID_KEY)? {
            return Ok(value);
        }

        let mut random = [0_u8; 24];
        SystemRandom::new()
            .fill(&mut random)
            .map_err(|_| ServerError::Internal {
                message: "operating-system randomness could not create a server instance ID"
                    .to_owned(),
            })?;
        let candidate = URL_SAFE_NO_PAD.encode(random);
        let connection = self.lock_connection()?;
        connection
            .execute(
                "INSERT OR IGNORE INTO server_properties(key, value) VALUES (?1, ?2)",
                [INSTANCE_ID_KEY, candidate.as_str()],
            )
            .map_err(|source| self.operation_error(source))?;
        connection
            .query_row(
                "SELECT value FROM server_properties WHERE key = ?1",
                [INSTANCE_ID_KEY],
                |row| row.get(0),
            )
            .map_err(|source| self.operation_error(source))
    }

    fn load_property(&self, key: &str) -> ServerResult<Option<String>> {
        self.lock_connection()?
            .query_row(
                "SELECT value FROM server_properties WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|source| self.operation_error(source))
    }
}

use rusqlite::OptionalExtension as _;
