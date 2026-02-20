use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SecretStoreError {
    #[error("secret not found")]
    NotFound,
    #[error("secret store unavailable: {0}")]
    Unavailable(String),
    #[error("secret store backend failure: {0}")]
    Backend(String),
}

pub trait SecretStore: Send + Sync {
    fn set_secret(
        &self,
        service: &str,
        account: &str,
        secret: &str,
    ) -> Result<(), SecretStoreError>;

    fn get_secret(&self, service: &str, account: &str) -> Result<String, SecretStoreError>;

    fn delete_secret(&self, service: &str, account: &str) -> Result<(), SecretStoreError>;
}

#[derive(Clone, Default)]
pub struct InMemorySecretStore {
    data: Arc<RwLock<HashMap<(String, String), String>>>,
}

impl SecretStore for InMemorySecretStore {
    fn set_secret(
        &self,
        service: &str,
        account: &str,
        secret: &str,
    ) -> Result<(), SecretStoreError> {
        let mut data = self
            .data
            .write()
            .map_err(|_| SecretStoreError::Backend("poisoned lock".to_owned()))?;
        data.insert((service.to_owned(), account.to_owned()), secret.to_owned());
        Ok(())
    }

    fn get_secret(&self, service: &str, account: &str) -> Result<String, SecretStoreError> {
        let data = self
            .data
            .read()
            .map_err(|_| SecretStoreError::Backend("poisoned lock".to_owned()))?;
        data.get(&(service.to_owned(), account.to_owned()))
            .cloned()
            .ok_or(SecretStoreError::NotFound)
    }

    fn delete_secret(&self, service: &str, account: &str) -> Result<(), SecretStoreError> {
        let mut data = self
            .data
            .write()
            .map_err(|_| SecretStoreError::Backend("poisoned lock".to_owned()))?;
        if data
            .remove(&(service.to_owned(), account.to_owned()))
            .is_none()
        {
            return Err(SecretStoreError::NotFound);
        }
        Ok(())
    }
}

#[cfg(feature = "os-keyring")]
#[derive(Default, Clone, Copy)]
pub struct OsKeyringSecretStore;

#[cfg(feature = "os-keyring")]
impl SecretStore for OsKeyringSecretStore {
    fn set_secret(
        &self,
        service: &str,
        account: &str,
        secret: &str,
    ) -> Result<(), SecretStoreError> {
        let entry = keyring::Entry::new(service, account)
            .map_err(|err| SecretStoreError::Backend(err.to_string()))?;
        entry
            .set_password(secret)
            .map_err(|err| SecretStoreError::Backend(err.to_string()))
    }

    fn get_secret(&self, service: &str, account: &str) -> Result<String, SecretStoreError> {
        let entry = keyring::Entry::new(service, account)
            .map_err(|err| SecretStoreError::Backend(err.to_string()))?;
        entry.get_password().map_err(|err| match err {
            keyring::Error::NoEntry => SecretStoreError::NotFound,
            other => SecretStoreError::Backend(other.to_string()),
        })
    }

    fn delete_secret(&self, service: &str, account: &str) -> Result<(), SecretStoreError> {
        let entry = keyring::Entry::new(service, account)
            .map_err(|err| SecretStoreError::Backend(err.to_string()))?;
        entry.delete_credential().map_err(|err| match err {
            keyring::Error::NoEntry => SecretStoreError::NotFound,
            other => SecretStoreError::Backend(other.to_string()),
        })
    }
}

#[derive(Clone)]
pub struct ScopedSecretStore<S: SecretStore> {
    inner: S,
    service: String,
}

impl<S: SecretStore> ScopedSecretStore<S> {
    pub fn new(inner: S, service: impl Into<String>) -> Self {
        Self {
            inner,
            service: service.into(),
        }
    }

    pub fn set(&self, account: &str, secret: &str) -> Result<(), SecretStoreError> {
        self.inner.set_secret(&self.service, account, secret)
    }

    pub fn get(&self, account: &str) -> Result<String, SecretStoreError> {
        self.inner.get_secret(&self.service, account)
    }

    pub fn delete(&self, account: &str) -> Result<(), SecretStoreError> {
        self.inner.delete_secret(&self.service, account)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_roundtrip() {
        let store = InMemorySecretStore::default();
        store
            .set_secret("pikachat", "@alice:example.org", "s3cr3t")
            .expect("set should work");

        let got = store
            .get_secret("pikachat", "@alice:example.org")
            .expect("get should work");
        assert_eq!(got, "s3cr3t");

        store
            .delete_secret("pikachat", "@alice:example.org")
            .expect("delete should work");
        assert_eq!(
            store.get_secret("pikachat", "@alice:example.org"),
            Err(SecretStoreError::NotFound)
        );
    }

    #[test]
    fn scoped_store_isolates_services() {
        let base = InMemorySecretStore::default();
        let a = ScopedSecretStore::new(base.clone(), "pikachat-a");
        let b = ScopedSecretStore::new(base.clone(), "pikachat-b");

        a.set("alice", "one").expect("set a");
        b.set("alice", "two").expect("set b");

        assert_eq!(a.get("alice").expect("get a"), "one");
        assert_eq!(b.get("alice").expect("get b"), "two");
    }

    #[derive(Default)]
    struct FailingStore;

    impl SecretStore for FailingStore {
        fn set_secret(
            &self,
            _service: &str,
            _account: &str,
            _secret: &str,
        ) -> Result<(), SecretStoreError> {
            Err(SecretStoreError::Unavailable("mock outage".to_owned()))
        }

        fn get_secret(&self, _service: &str, _account: &str) -> Result<String, SecretStoreError> {
            Err(SecretStoreError::Unavailable("mock outage".to_owned()))
        }

        fn delete_secret(&self, _service: &str, _account: &str) -> Result<(), SecretStoreError> {
            Err(SecretStoreError::Unavailable("mock outage".to_owned()))
        }
    }

    #[test]
    fn mock_failure_propagates_through_scoped_store() {
        let scoped = ScopedSecretStore::new(FailingStore, "pikachat");
        let err = scoped.set("alice", "secret").expect_err("set must fail");
        assert_eq!(err, SecretStoreError::Unavailable("mock outage".to_owned()));
    }
}
