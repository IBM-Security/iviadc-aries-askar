use std::fmt::{self, Debug, Formatter};
use std::sync::Arc;
use std::collections::BTreeMap;

use odbc_api::{
    buffers::{RowVec},
    Cursor,
    IntoParameter,
    parameter::{VarCharArray}
};

use odbc_api::sys;

use super::{
    db_utils::{expiry_timestamp, random_profile_name, encode_profile_key, DbSessionKey, prepare_tags},
    Backend, BackendSession,
};
use crate::{
    backend::OrderBy,
    entry::{Entry, EntryKind, EntryOperation, EntryTag, Scan, TagFilter},
    error::Error,
    future::{BoxFuture, unblock},
    protect::{EntryEncryptor, KeyCache, PassKey, ProfileId, ProfileKey, StoreKeyMethod},
};

use r2d2::PooledConnection;

mod provision;
pub use self::provision::OdbcStoreOptions;

mod r2d2_connection_pool;
use crate::odbc::r2d2_connection_pool::OdbcConnectionManager;

// All of our SQL queries.  Each of these queries conform to the SQL-92 standard.
const UPDATE_CONFIG_PROFILE: &str = "UPDATE config SET value = ? WHERE name='default_profile'";
const UPDATE_CONFIG_KEY: &str = "UPDATE config SET value=? WHERE name='key'";
const GET_DEFAULT_PROFILE: &str = "SELECT value FROM config WHERE name='default_profile'";

const GET_PROFILE_ID: &str = "SELECT id from profiles WHERE name=? and profile_key=?";
const GET_PROFILE_NAMES: &str = "SELECT name FROM profiles";
const GET_PROFILE_COUNT_FOR_NAME: &str = "SELECT COUNT(name) from profiles WHERE name=?";
const GET_PROFILES: &str = "SELECT id, profile_key FROM profiles";
const GET_PROFILE: &str = "SELECT id, profile_key FROM profiles WHERE name=?";
const INSERT_PROFILE: &str = "INSERT INTO profiles (name, profile_key) VALUES (?, ?)";
const UPDATE_PROFILE: &str = "UPDATE profiles SET profile_key=? WHERE id=?";
const DELETE_PROFILE: &str = "DELETE FROM profiles WHERE name=?";

const GET_ITEM_ID: &str = "SELECT id FROM items WHERE profile_id=? AND kind=? AND category=? AND name=?";
const INSERT_ITEM: &str = "INSERT INTO items (profile_id, kind, category, name, value, expiry) VALUES (?, ?, ?, ?, ?, NULL)";
const INSERT_ITEM_WITH_EXPIRY: &str = "INSERT INTO items (profile_id, kind, category, name, value, expiry) VALUES (?, ?, ?, ?, ?, ?)";
const UPDATE_ITEM: &str = "UPDATE items SET value=?, expiry=NULL WHERE profile_id=? AND kind=?
    AND category=? AND name=?";
const UPDATE_ITEM_WITH_EXPIRY: &str = "UPDATE items SET value=?, expiry=? WHERE profile_id=? AND kind=?
    AND category=? AND name=?";
const DELETE_ITEM: &str = "DELETE FROM items WHERE profile_id = ? AND kind = ? AND category = ? AND name = ?";

const INSERT_TAG: &str = "INSERT INTO items_tags (item_id, name, value, plaintext) VALUES (?, ?, ?, ?)";
const DELETE_TAG: &str = "DELETE FROM items_tags WHERE item_id=?";

/// A ODBC database store
pub struct OdbcBackend {
    pool: r2d2::Pool<OdbcConnectionManager>,
    active_profile: String,
    key_cache: Arc<KeyCache>,
}

impl OdbcBackend {
    pub(crate) fn new(
        pool: r2d2::Pool<OdbcConnectionManager>,
        active_profile: String,
        key_cache: KeyCache,
    ) -> Self {
        Self {
            pool,
            active_profile,
            key_cache: Arc::new(key_cache),
        }
    }
}

impl Backend for OdbcBackend {
    type Session = OdbcSession;

    fn create_profile(&self, name: Option<String>) -> BoxFuture<'_, Result<String, Error>> {
        let name = name.unwrap_or_else(random_profile_name);

        Box::pin(async move {
            // Create the profile key.
            let store_key = self.key_cache.store_key.clone();
            let (profile_key, enc_key) = unblock(move || {
                let profile_key = ProfileKey::new()?;
                let enc_key = encode_profile_key(&profile_key, &store_key)?;
                Result::<_, Error>::Ok((profile_key, enc_key))
            })
            .await?;

            // Store the profile name and key.
            self.pool.get().unwrap().raw().execute(INSERT_PROFILE,
                (&name.clone().into_parameter(), &enc_key.clone().into_parameter()))?;

            // Retrieve the profile ID from the table.
            let mut pid: i64 = 0;

            self.pool.get().unwrap().raw().execute(GET_PROFILE_ID,
                (&name.clone().into_parameter(), &enc_key.clone().into_parameter()))
            .unwrap().unwrap()
            .next_row().unwrap().unwrap()
            .get_data(1, &mut pid)?;

            // Add the details to the key cache.
            self.key_cache
                    .add_profile(name.clone(), pid, Arc::new(profile_key))
                    .await;

            Ok(name)
        })
    }

    fn get_active_profile(&self) -> String {
        self.active_profile.clone()
    }

    fn get_default_profile(&self) -> BoxFuture<'_, Result<String, Error>> {
        Box::pin(async move {
            let mut profile_buf = Vec::new();

            self.pool.get().unwrap().raw().execute(GET_DEFAULT_PROFILE, ())
                .unwrap().unwrap()
                .next_row().unwrap().unwrap()
                .get_text(1, &mut profile_buf)?;

            Ok(String::from_utf8(profile_buf).unwrap())
        })
    }

    fn set_default_profile(&self, profile: String) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move {
            self.pool.get().unwrap().raw().execute(UPDATE_CONFIG_PROFILE,
                    (&profile.into_parameter()))?;
            Ok(())
        })
    }

    fn list_profiles(&self) -> BoxFuture<'_, Result<Vec<String>, Error>> {
        Box::pin(async move {
            let mut names: Vec<String> = Vec::new();

            match self.pool.get().unwrap().raw().execute(GET_PROFILE_NAMES, ()) {
                Ok(cursor) => {
                    let row_set_buffer = RowVec::<(VarCharArray<1024>,)>::new(10);
                    let mut block_cursor = cursor.unwrap().bind_buffer(row_set_buffer).unwrap();
                    let batch = block_cursor.fetch().unwrap().unwrap();

                    for idx in 0..batch.num_rows() {
                        names.push(batch[idx].0.as_str().unwrap().unwrap().to_string());
                    }
                }
                Err(_error) => {
                    return Err(err_msg!(Unsupported, "Configuration data not found"));
                }
            };
            Ok(names)
        })
    }

    fn remove_profile(&self, name: String) -> BoxFuture<'_, Result<bool, Error>> {
        Box::pin(async move {
            let mut ret = false;

            // Determine whether the profile currently exists.  We use this to
            // determine whether to delete the profile, along with the return
            // value from this function (true == deleted / false == unknown profile).
            let mut count: i64 = 0;

            self.pool.get().unwrap().raw().execute(GET_PROFILE_COUNT_FOR_NAME,
                        (&name.clone().into_parameter()))
                .unwrap().unwrap()
                .next_row().unwrap().unwrap()
                .get_data(1, &mut count)?;

            if count > 0 {
                self.pool.get().unwrap().raw().execute(DELETE_PROFILE,
                    (&name.into_parameter()))?;

                ret = true;
            }

            Ok(ret)
        })
    }

    fn rekey(
        &mut self,
        method: StoreKeyMethod,
        pass_key: PassKey<'_>,
    ) -> BoxFuture<'_, Result<(), Error>> {
        let pass_key = pass_key.into_owned();

        Box::pin(async move {
            let (store_key, store_key_ref) = unblock(move || method.resolve(pass_key)).await?;
            let store_key = Arc::new(store_key);
            let binding = self.pool.get().unwrap();
            let mut upd_keys = BTreeMap::<ProfileId, Vec<u8>>::new();

            // Retrieve and temporarily store the current keys for each
            // of the profiles.
            match binding.raw().execute(GET_PROFILES, ()) {
                Ok(cursor) => {
                    let mut unwrapped = cursor.unwrap();

                    while let Some(mut row) = unwrapped.next_row()? {
                        let mut pid: i64 = 0;
                        let mut enc_key = Vec::new();

                        row.get_data(1, &mut pid)?;
                        row.get_binary(2, &mut enc_key).unwrap();

                        upd_keys.insert(pid, enc_key);
                    }
                }
                Err(_error) => {
                    return Err(err_msg!(Unsupported, "Configuration data not found"));
                }
            };

            // Iterate over the cached keys, updating the profile with the new
            // key.
            for (pid, key) in upd_keys {
                let profile_key = self.key_cache.load_key(key).await?;
                let upd_key = unblock({
                    let store_key = store_key.clone();
                    move || encode_profile_key(&profile_key, &store_key)
                })
                .await?;

                binding.raw().execute(UPDATE_PROFILE,
                    (&upd_key.into_parameter(), &pid.into_parameter()))?;
            }

            // We finally need to save the new store key.
            binding.raw().execute(UPDATE_CONFIG_KEY,
                    (&store_key_ref.into_uri().into_parameter()))?;

            Ok(())
        })
    }

    fn scan(
        &self,
        profile: Option<String>,
        kind: Option<EntryKind>,
        category: Option<String>,
        tag_filter: Option<TagFilter>,
        offset: Option<i64>,
        limit: Option<i64>,
        order_by: Option<OrderBy>,
        descending: bool,
    ) -> BoxFuture<'_, Result<Scan<'static, Entry>, Error>> {
        // XXX: Still to be done
        Box::pin(async move { Err(err_msg!(Unsupported, "mod::scan()")) })
    }

    fn session(&self, profile: Option<String>, transaction: bool) -> Result<Self::Session, Error> {
        if transaction {
            // XXX: Still to be done
            return Err(err_msg!(Unsupported, "The ODBC backend does not currently support transactions"))
        }
        Ok(OdbcSession::new(
            self.key_cache.clone(),
            profile.unwrap_or_else(|| self.active_profile.clone()),
            self.pool.get().unwrap(),
        ))
    }

    fn close(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move { Ok(()) })
    }
}

impl Debug for OdbcBackend {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("OdbcStore")
            .field("active_profile", &self.active_profile)
            .finish()
    }
}

/// A ODBC session
#[derive(Debug)]
pub struct OdbcSession {
    cache: Arc<KeyCache>,
    profile: String,
    connection: PooledConnection<OdbcConnectionManager>,
}

impl OdbcSession {
    pub(crate) fn new(
        cache: Arc<KeyCache>,
        profile: String,
        connection: PooledConnection<OdbcConnectionManager>,
    ) -> Self
    {
        Self {
            cache: cache,
            profile: profile,
            connection: connection,
        }
    }

    async fn acquire_key(&mut self) -> Result<(ProfileId, Arc<ProfileKey>), Error> {
        // Check to see whether the key already exists in our cache...
        if let Some((pid, key)) = self.cache.get_profile(self.profile.as_str()).await {
            Ok((pid, key))
        } else {
            // The key isn't already cached and so we need to try and load the key
            // from the database.
            let mut pid: i64 = 0;
            let mut enc_key = Vec::new();

            if let Some(mut cursor) = self.connection.raw().execute(GET_PROFILE, (&self.profile.clone().into_parameter()))?
            {
                let mut row = cursor.next_row().unwrap().unwrap();
                row.get_data(1, &mut pid)?;
                row.get_binary(2, &mut enc_key)?;
            } else {
                return Err(err_msg!(NotFound, "Profile not found"));
            }

            // Load and cache the key.
            let key = Arc::new(self.cache.load_key(enc_key).await?);
            self.cache.add_profile(self.profile.clone(), pid, key.clone()).await;

            Ok((pid, key))
        }
    }
}

impl BackendSession for OdbcSession {
    fn count<'q>(
        &'q mut self,
        kind: Option<EntryKind>,
        category: Option<&'q str>,
        tag_filter: Option<TagFilter>,
    ) -> BoxFuture<'q, Result<i64, Error>> {
        // XXX: Still to be done
        let enc_category = category.map(|c| ProfileKey::prepare_input(c.as_bytes()));

        Box::pin(async move { Ok(5) })
    }

    fn fetch(
        &mut self,
        kind: EntryKind,
        category: &str,
        name: &str,
        for_update: bool,
    ) -> BoxFuture<'_, Result<Option<Entry>, Error>> {
        // XXX: Still to be done
        let category = category.to_string();
        let name = name.to_string();

        Box::pin(async move { Ok(None) })
    }

    fn fetch_all<'q>(
        &'q mut self,
        kind: Option<EntryKind>,
        category: Option<&'q str>,
        tag_filter: Option<TagFilter>,
        limit: Option<i64>,
        order_by: Option<OrderBy>,
        descending: bool,
        for_update: bool,
    ) -> BoxFuture<'q, Result<Vec<Entry>, Error>> {
        // XXX: Still to be done
        let category = category.map(|c| c.to_string());
        Box::pin(async move { Err(err_msg!(Unsupported, "mod::fetch_all()")) })
    }

    fn remove_all<'q>(
        &'q mut self,
        kind: Option<EntryKind>,
        category: Option<&'q str>,
        tag_filter: Option<TagFilter>,
    ) -> BoxFuture<'q, Result<i64, Error>> {
        // XXX: Still to be done
        let enc_category = category.map(|c| ProfileKey::prepare_input(c.as_bytes()));

        Box::pin(async move { Err(err_msg!(Unsupported, "mod::remove_all()")) })
    }

    fn update<'q>(
        &'q mut self,
        kind: EntryKind,
        operation: EntryOperation,
        category: &'q str,
        name: &'q str,
        value: Option<&'q [u8]>,
        tags: Option<&'q [EntryTag]>,
        expiry_ms: Option<i64>,
    ) -> BoxFuture<'q, Result<(), Error>> {
        let category = ProfileKey::prepare_input(category.as_bytes());
        let name = ProfileKey::prepare_input(name.as_bytes());

        // XXX: Can we use a transaction here???
        match operation {
            op @ EntryOperation::Insert | op @ EntryOperation::Replace => {
                let value = ProfileKey::prepare_input(value.unwrap_or_default());
                let tags = tags.map(prepare_tags);
                Box::pin(async move {
                    // Locate the correct key and then encrypt our various fields.
                    let (pid, key) = self.acquire_key().await?;
                    let (enc_category, enc_name, enc_value, enc_tags) = unblock(move || {
                        let enc_value =
                            key.encrypt_entry_value(category.as_ref(), name.as_ref(), value)?;
                        Result::<_, Error>::Ok((
                            key.encrypt_entry_category(category)?,
                            key.encrypt_entry_name(name)?,
                            enc_value,
                            tags.transpose()?
                                .map(|t| key.encrypt_entry_tags(t))
                                .transpose()?,
                        ))
                    })
                    .await?;

                    let mut statement = self.connection.raw().preallocate().unwrap();

                    // Work out the expiry time.
                    let mut expiryStr: String = String::new();

                    if let Some(mut expiry) = expiry_ms.map(expiry_timestamp).transpose()? {
                        // ODBC expects the time stamp to be in a string, of the format:
                        //   YYYY-MM-DD HH:MM:SS.MSEC
                        expiryStr = format!("{}", expiry.format("%Y-%m-%d %H:%M:%S.%6f"));
                    }

                    // Now we need to store the fields in the database.
                    if op == EntryOperation::Insert {
                        if expiryStr.is_empty() {
                            statement.execute(INSERT_ITEM,
                                (
                                    &pid.into_parameter(),
                                    &(kind as i16).into_parameter(),
                                    &enc_category.clone().into_parameter(),
                                    &enc_name.clone().into_parameter(),
                                    &enc_value.into_parameter()
                                ))?;
                        } else {
                            statement.execute(INSERT_ITEM_WITH_EXPIRY,
                                (
                                    &pid.into_parameter(),
                                    &(kind as i16).into_parameter(),
                                    &enc_category.clone().into_parameter(),
                                    &enc_name.clone().into_parameter(),
                                    &enc_value.into_parameter(),
                                    &expiryStr.into_parameter()
                                ))?;
                        }
                    } else {
                        if expiryStr.is_empty() {
                            statement.execute(UPDATE_ITEM,
                                (
                                    &enc_value.into_parameter(),
                                    &pid.into_parameter(),
                                    &(kind as i16).into_parameter(),
                                    &enc_category.clone().into_parameter(),
                                    &enc_name.clone().into_parameter()
                                ))?;
                        } else {
                            statement.execute(UPDATE_ITEM_WITH_EXPIRY,
                                (
                                    &enc_value.into_parameter(),
                                    &expiryStr.into_parameter(),
                                    &pid.into_parameter(),
                                    &(kind as i16).into_parameter(),
                                    &enc_category.clone().into_parameter(),
                                    &enc_name.clone().into_parameter()
                                ))?;
                        }

                        // We also want to delete all existing tags for this
                        // item.

                        statement.execute(DELETE_TAG,
                            (&pid.into_parameter()))?;
                    }

                    // Now we need to update the tags table.
                    if let Some(tags) = enc_tags {
                        // Retrieve the item identifier.
                        let mut item_id: i64 = 0;

                        statement.execute(GET_ITEM_ID,
                            (
                                &pid.into_parameter(),
                                &(kind as i16).into_parameter(),
                                &enc_category.clone().into_parameter(),
                                &enc_name.clone().into_parameter()
                            ))
                            .unwrap().unwrap()
                            .next_row().unwrap().unwrap()
                            .get_data(1, &mut item_id)?;

                        // Update each of the tags.
                        let mut prepared = self.connection.raw().prepare(INSERT_TAG).unwrap();

                        for tag in tags {
                            prepared.execute(
                                (
                                    &item_id.into_parameter(),
                                    &tag.name.into_parameter(),
                                    &tag.value.into_parameter(),
                                    &(tag.plaintext as i16).into_parameter()
                                ))?;
                        }
                    }

                    Ok(())
                })
            }

            EntryOperation::Remove => Box::pin(async move {
                // Create the encrypted category and name.
                let (pid, key) = self.acquire_key().await?;
                let (enc_category, enc_name) = unblock(move || {
                    Result::<_, Error>::Ok((
                        key.encrypt_entry_category(category)?,
                        key.encrypt_entry_name(name)?,
                    ))
                })
                .await?;

                // Issue the delete.  We don't return an error if the
                // item doesn't currently exist.
                self.connection.raw().execute(DELETE_ITEM,
                    (
                        &pid.into_parameter(),
                        &(kind as i16).into_parameter(),
                        &enc_category.into_parameter(),
                        &enc_name.into_parameter()
                    ))?;

                Ok(())
            }),
        }
    }

    fn ping(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        // XXX: Still to be done
        Box::pin(async move {
            Ok(())
        })
    }

    fn close(&mut self, commit: bool) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(self.close(commit))
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::db_utils::replace_arg_placeholders;

    /*
    #[test]
    fn odbc_simple_and_convert_args_works() {
        assert_eq!(
            &replace_arg_placeholders::<OdbcBackend>("This $$ is $10 a $$ string!", 3),
            "This $3 is $12 a $5 string!",
        );
    }
    */
}
