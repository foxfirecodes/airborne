//! Credential storage for the Airborne CLI.
//!
//! This crate only reads and writes macOS Keychain items. The CLI decides
//! whether an environment credential takes precedence.

use std::{collections::HashMap, sync::Mutex};

pub use secrecy::{ExposeSecret, SecretString};

/// The Keychain service used for credentials managed by Airborne.
pub const AIRBORNE_KEYCHAIN_SERVICE: &str = "airborne";

/// The Keychain service used by the desktop prototype.
pub const LEGACY_KEYCHAIN_SERVICE: &str = "com.prwatcher.v0";

/// A provider with a credential managed by Airborne.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ProviderCredential {
    GitHub,
    Buildkite,
}

impl ProviderCredential {
    /// The fixed Keychain account name for this provider.
    #[must_use]
    pub const fn account_name(self) -> &'static str {
        match self {
            Self::GitHub => "github_token",
            Self::Buildkite => "buildkite_token",
        }
    }

    #[must_use]
    const fn display_name(self) -> &'static str {
        match self {
            Self::GitHub => "GitHub",
            Self::Buildkite => "Buildkite",
        }
    }
}

/// A credential operation that failed without exposing secret data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialOperation {
    Read,
    Write,
    Remove,
}

impl CredentialOperation {
    const fn action(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Remove => "remove",
        }
    }
}

/// Errors from credential storage. These errors deliberately retain no source
/// error because Keychain errors can include private item details.
#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum CredentialError {
    #[error("{provider} credential is not configured")]
    Missing { provider: ProviderCredential },
    #[error("could not {operation} the {provider} credential in macOS Keychain")]
    Keychain {
        provider: ProviderCredential,
        operation: CredentialOperation,
    },
    #[error("credential value must not be empty")]
    EmptyValue,
    #[error("the in-memory credential store is unavailable")]
    InMemoryUnavailable,
}

impl std::fmt::Display for ProviderCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.display_name())
    }
}

impl std::fmt::Display for CredentialOperation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.action())
    }
}

/// The credential port used by the CLI and provider composition.
pub trait CredentialStore: Send + Sync {
    /// Returns the stored credential for `provider`.
    ///
    /// # Errors
    ///
    /// Returns a redacted error when the credential is missing or the backing
    /// store cannot be accessed.
    fn get(&self, provider: ProviderCredential) -> Result<SecretString, CredentialError>;

    /// Stores a non-empty credential for `provider`.
    ///
    /// # Errors
    ///
    /// Returns a redacted error when the value is empty or the backing store
    /// cannot be written.
    fn set(&self, provider: ProviderCredential, value: SecretString)
        -> Result<(), CredentialError>;

    /// Removes Airborne's item. Removing an item that is already absent succeeds.
    ///
    /// # Errors
    ///
    /// Returns a redacted error when the backing store cannot be accessed.
    fn remove(&self, provider: ProviderCredential) -> Result<(), CredentialError>;
}

/// The presence of a credential without revealing it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialPresence {
    Present,
    Missing,
}

/// The production macOS Keychain-backed credential store.
#[derive(Clone, Copy, Debug, Default)]
pub struct MacosCredentialStore;

impl MacosCredentialStore {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Returns whether Airborne has a credential, while preserving access
    /// failures for the caller to report safely.
    ///
    /// # Errors
    ///
    /// Returns a redacted error when Keychain cannot be accessed.
    pub fn presence(
        &self,
        provider: ProviderCredential,
    ) -> Result<CredentialPresence, CredentialError> {
        match Self::read_from_service(AIRBORNE_KEYCHAIN_SERVICE, provider) {
            Ok(_) => Ok(CredentialPresence::Present),
            Err(CredentialError::Missing { .. }) => Ok(CredentialPresence::Missing),
            Err(error) => Err(error),
        }
    }

    /// Reads a legacy prototype credential without changing it.
    ///
    /// # Errors
    ///
    /// Returns a redacted error when Keychain cannot be accessed.
    pub fn get_legacy(
        &self,
        provider: ProviderCredential,
    ) -> Result<Option<SecretString>, CredentialError> {
        match Self::read_from_service(LEGACY_KEYCHAIN_SERVICE, provider) {
            Ok(value) => Ok(Some(value)),
            Err(CredentialError::Missing { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Copies a legacy prototype credential into Airborne's service.
    ///
    /// The old Keychain item remains untouched. `Ok(false)` means there was no
    /// legacy item to copy.
    ///
    /// # Errors
    ///
    /// Returns a redacted error when either Keychain operation fails.
    pub fn copy_legacy(&self, provider: ProviderCredential) -> Result<bool, CredentialError> {
        let Some(value) = self.get_legacy(provider)? else {
            return Ok(false);
        };
        self.set(provider, value)?;
        Ok(true)
    }

    fn read_from_service(
        service: &str,
        provider: ProviderCredential,
    ) -> Result<SecretString, CredentialError> {
        let entry = keyring::Entry::new(service, provider.account_name()).map_err(|_| {
            CredentialError::Keychain {
                provider,
                operation: CredentialOperation::Read,
            }
        })?;
        entry
            .get_password()
            .map(SecretString::from)
            .map_err(|error| {
                if matches!(error, keyring::Error::NoEntry) {
                    CredentialError::Missing { provider }
                } else {
                    CredentialError::Keychain {
                        provider,
                        operation: CredentialOperation::Read,
                    }
                }
            })
    }
}

impl CredentialStore for MacosCredentialStore {
    fn get(&self, provider: ProviderCredential) -> Result<SecretString, CredentialError> {
        Self::read_from_service(AIRBORNE_KEYCHAIN_SERVICE, provider)
    }

    fn set(
        &self,
        provider: ProviderCredential,
        value: SecretString,
    ) -> Result<(), CredentialError> {
        if value.expose_secret().is_empty() {
            return Err(CredentialError::EmptyValue);
        }
        let entry = keyring::Entry::new(AIRBORNE_KEYCHAIN_SERVICE, provider.account_name())
            .map_err(|_| CredentialError::Keychain {
                provider,
                operation: CredentialOperation::Write,
            })?;
        entry
            .set_password(value.expose_secret())
            .map_err(|_| CredentialError::Keychain {
                provider,
                operation: CredentialOperation::Write,
            })
    }

    fn remove(&self, provider: ProviderCredential) -> Result<(), CredentialError> {
        let entry = keyring::Entry::new(AIRBORNE_KEYCHAIN_SERVICE, provider.account_name())
            .map_err(|_| CredentialError::Keychain {
                provider,
                operation: CredentialOperation::Remove,
            })?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(CredentialError::Keychain {
                provider,
                operation: CredentialOperation::Remove,
            }),
        }
    }
}

/// An in-memory credential store that never accesses macOS Keychain.
///
/// Tests and alternate hosts can use this store directly.
#[derive(Debug, Default)]
pub struct InMemoryCredentialStore {
    values: Mutex<HashMap<ProviderCredential, SecretString>>,
}

impl InMemoryCredentialStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_credential(provider: ProviderCredential, value: SecretString) -> Self {
        let mut values = HashMap::new();
        values.insert(provider, value);
        Self {
            values: Mutex::new(values),
        }
    }

    /// Returns whether a credential is present without exposing it.
    ///
    /// # Errors
    ///
    /// Returns an error only when an earlier caller poisoned the store lock.
    pub fn presence(
        &self,
        provider: ProviderCredential,
    ) -> Result<CredentialPresence, CredentialError> {
        let values = self
            .values
            .lock()
            .map_err(|_| CredentialError::InMemoryUnavailable)?;
        Ok(if values.contains_key(&provider) {
            CredentialPresence::Present
        } else {
            CredentialPresence::Missing
        })
    }
}

impl CredentialStore for InMemoryCredentialStore {
    fn get(&self, provider: ProviderCredential) -> Result<SecretString, CredentialError> {
        let values = self
            .values
            .lock()
            .map_err(|_| CredentialError::InMemoryUnavailable)?;
        values
            .get(&provider)
            .cloned()
            .ok_or(CredentialError::Missing { provider })
    }

    fn set(
        &self,
        provider: ProviderCredential,
        value: SecretString,
    ) -> Result<(), CredentialError> {
        if value.expose_secret().is_empty() {
            return Err(CredentialError::EmptyValue);
        }
        self.values
            .lock()
            .map_err(|_| CredentialError::InMemoryUnavailable)?
            .insert(provider, value);
        Ok(())
    }

    fn remove(&self, provider: ProviderCredential) -> Result<(), CredentialError> {
        self.values
            .lock()
            .map_err(|_| CredentialError::InMemoryUnavailable)?
            .remove(&provider);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "token-that-must-never-appear-in-errors";

    #[test]
    fn provider_accounts_are_fixed() {
        assert_eq!(ProviderCredential::GitHub.account_name(), "github_token");
        assert_eq!(
            ProviderCredential::Buildkite.account_name(),
            "buildkite_token"
        );
    }

    #[test]
    fn secret_and_errors_redact_tokens() {
        let secret = SecretString::from(TOKEN);
        let error = CredentialError::Keychain {
            provider: ProviderCredential::GitHub,
            operation: CredentialOperation::Write,
        };

        assert!(!format!("{secret:?}").contains(TOKEN));
        assert!(!error.to_string().contains(TOKEN));
        assert!(!format!("{error:?}").contains(TOKEN));
    }

    #[test]
    fn in_memory_store_round_trips_without_keychain() {
        let store = InMemoryCredentialStore::new();
        assert_eq!(
            store.presence(ProviderCredential::GitHub),
            Ok(CredentialPresence::Missing)
        );

        store
            .set(ProviderCredential::GitHub, SecretString::from(TOKEN))
            .unwrap();
        assert_eq!(
            store.presence(ProviderCredential::GitHub),
            Ok(CredentialPresence::Present)
        );
        assert_eq!(
            store
                .get(ProviderCredential::GitHub)
                .unwrap()
                .expose_secret(),
            TOKEN
        );

        store.remove(ProviderCredential::GitHub).unwrap();
        store.remove(ProviderCredential::GitHub).unwrap();
        assert!(matches!(
            store.get(ProviderCredential::GitHub),
            Err(CredentialError::Missing {
                provider: ProviderCredential::GitHub
            })
        ));
    }

    #[test]
    fn in_memory_store_rejects_empty_values() {
        let store = InMemoryCredentialStore::new();
        assert_eq!(
            store.set(ProviderCredential::Buildkite, SecretString::from("")),
            Err(CredentialError::EmptyValue)
        );
    }
}
