use crate::paths::{self, CookieLocation};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::ambient_authority;
#[cfg(unix)]
use cap_std::fs::OpenOptionsExt as _;
use cap_std::fs::{Dir, OpenOptions};
use cookie::Cookie;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

const SESSION_COOKIE_PREFIX: &str = "REVEL_SESSION=";
// RFC 6265 user agents are expected to support at least 4096 bytes per
// cookie. AtCoder's session cookie is much smaller; this cap also prevents an
// accidentally large file from being read into memory or used as a header.
const MAX_COOKIE_LINE_BYTES: usize = 4096;
const AUTH_LOCK_FILE: &str = ".cookie.lock";
const AUTH_STAGING_PREFIX: &str = ".cookie-staging-";

static PROCESS_AUTH_STORE_LOCK: Mutex<()> = Mutex::new(());
static NEXT_AUTH_STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) struct Credential {
    value: String,
}

impl Credential {
    pub(crate) fn as_str(&self) -> &str {
        &self.value
    }

    fn from_cookie_value(value: &str) -> io::Result<Self> {
        if value.is_empty()
            || value.len() + SESSION_COOKIE_PREFIX.len() > MAX_COOKIE_LINE_BYTES
            || !value.bytes().all(is_cookie_octet)
        {
            return Err(invalid_cookie_file_error());
        }
        Ok(Self {
            value: format!("{SESSION_COOKIE_PREFIX}{value}"),
        })
    }

    fn same_value(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct PastedCredentialError;

impl fmt::Debug for PastedCredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PastedCredentialError(<redacted>)")
    }
}

impl fmt::Display for PastedCredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("enter one valid REVEL_SESSION value")
    }
}

impl std::error::Error for PastedCredentialError {}

/// Parse input copied by a user. This is deliberately separate from both the on-disk record
/// parser and Set-Cookie parsing: only a bare value or one optional REVEL_SESSION prefix is
/// accepted, and no general whitespace normalization is performed.
pub(crate) fn parse_pasted_credential(input: &str) -> Result<Credential, PastedCredentialError> {
    let input = input
        .strip_suffix("\r\n")
        .or_else(|| input.strip_suffix('\n'))
        .unwrap_or(input);
    let value = input.strip_prefix(SESSION_COOKIE_PREFIX).unwrap_or(input);

    if input
        .get(.."Cookie:".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("Cookie:"))
        || value.starts_with(SESSION_COOKIE_PREFIX)
    {
        return Err(PastedCredentialError);
    }

    Credential::from_cookie_value(value).map_err(|_| PastedCredentialError)
}

impl fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Credential(<redacted>)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthLoadErrorKind {
    InvalidFormat,
    UnsafeFilesystemState,
    Permission,
    Io,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AuthLoadError {
    kind: AuthLoadErrorKind,
}

impl AuthLoadError {
    pub(crate) fn from_io(error: &io::Error) -> Self {
        let kind = match error.kind() {
            io::ErrorKind::InvalidData => AuthLoadErrorKind::InvalidFormat,
            io::ErrorKind::InvalidInput => AuthLoadErrorKind::UnsafeFilesystemState,
            io::ErrorKind::PermissionDenied => AuthLoadErrorKind::Permission,
            _ => AuthLoadErrorKind::Io,
        };
        Self { kind }
    }

    #[cfg(test)]
    pub(crate) fn kind(self) -> AuthLoadErrorKind {
        self.kind
    }
}

impl fmt::Display for AuthLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.kind {
            AuthLoadErrorKind::InvalidFormat => "authentication cookie format is invalid",
            AuthLoadErrorKind::UnsafeFilesystemState => {
                "authentication cookie filesystem state is unsafe"
            }
            AuthLoadErrorKind::Permission => "authentication cookie permissions are unsafe",
            AuthLoadErrorKind::Io => "authentication cookie could not be read safely",
        })
    }
}

impl std::error::Error for AuthLoadError {}

pub(crate) enum AuthSnapshot {
    Configured {
        credential: Arc<Credential>,
        store: Option<Arc<AuthStore>>,
    },
    Missing,
    Invalid(AuthLoadError),
}

#[derive(Clone)]
pub(crate) struct CredentialIdentity(Arc<Credential>);

impl fmt::Debug for CredentialIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CredentialIdentity(<redacted>)")
    }
}

#[derive(Clone)]
enum AuthenticationRuntimeIdentity {
    Configured(CredentialIdentity),
    ServerDeleted,
}

impl fmt::Debug for AuthenticationRuntimeIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Configured(_) => "Configured(<redacted>)",
            Self::ServerDeleted => "ServerDeleted",
        })
    }
}

/// Opaque identity for the credential lineage observed by one authentication request.
///
/// This deliberately keeps the credential itself private and redacted. A verification result may
/// be applied to either its bootstrap credential or the final server-driven successor. That also
/// covers a rotation whose persistence failed and left the bootstrap credential on disk.
#[derive(Clone)]
pub(crate) struct AuthenticationLineage {
    bootstrap: CredentialIdentity,
    runtime: AuthenticationRuntimeIdentity,
}

impl fmt::Debug for AuthenticationLineage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthenticationLineage")
            .field("bootstrap", &self.bootstrap)
            .field("runtime", &self.runtime)
            .finish()
    }
}

impl AuthenticationLineage {
    pub(crate) fn matches_snapshot(&self, snapshot: &AuthSnapshot) -> bool {
        match snapshot {
            AuthSnapshot::Configured { credential, .. } => {
                credential.same_value(&self.bootstrap.0)
                    || matches!(
                        &self.runtime,
                        AuthenticationRuntimeIdentity::Configured(identity)
                            if credential.same_value(&identity.0)
                    )
            }
            AuthSnapshot::Missing => {
                matches!(self.runtime, AuthenticationRuntimeIdentity::ServerDeleted)
            }
            AuthSnapshot::Invalid(_) => false,
        }
    }
}

impl fmt::Debug for AuthSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configured { credential, .. } => formatter
                .debug_tuple("Configured")
                .field(credential)
                .finish(),
            Self::Missing => formatter.write_str("Missing"),
            Self::Invalid(error) => formatter.debug_tuple("Invalid").field(error).finish(),
        }
    }
}

impl AuthSnapshot {
    pub(crate) fn load() -> Arc<Self> {
        let snapshot = match paths::cookie_location() {
            Ok(location) => load_auth_snapshot_from(&location),
            Err(error) => Self::Invalid(AuthLoadError::from_io(&io::Error::other(error))),
        };
        Arc::new(snapshot)
    }

    pub(crate) fn load_from(location: &CookieLocation) -> Arc<Self> {
        Arc::new(load_auth_snapshot_from(location))
    }

    #[cfg(test)]
    pub(crate) fn credential(&self) -> Option<&Credential> {
        match self {
            Self::Configured { credential, .. } => Some(credential),
            Self::Missing | Self::Invalid(_) => None,
        }
    }

    fn store(&self) -> Option<Arc<AuthStore>> {
        match self {
            Self::Configured { store, .. } => store.clone(),
            Self::Missing | Self::Invalid(_) => None,
        }
    }

    fn credential_identity(&self) -> Option<CredentialIdentity> {
        match self {
            Self::Configured { credential, .. } => Some(CredentialIdentity(Arc::clone(credential))),
            Self::Missing | Self::Invalid(_) => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn submission_unavailable_message(&self) -> Option<&'static str> {
        match self {
            Self::Configured { .. } => None,
            Self::Missing => {
                Some("Authentication is not configured.\nReturn Home to configure authentication.")
            }
            Self::Invalid(_) => {
                Some("Authentication configuration is invalid.\nReturn Home to fix authentication.")
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn configured_for_test(value: &str) -> Arc<Self> {
        Arc::new(Self::Configured {
            credential: Arc::new(parse_cookie_file(value).expect("valid test credential")),
            store: None,
        })
    }
}

fn load_auth_snapshot_from(location: &CookieLocation) -> AuthSnapshot {
    match AuthStore::open(location) {
        Ok(Some(store)) => match store.load_current() {
            Ok(Some(credential)) => AuthSnapshot::Configured {
                credential: Arc::new(credential),
                store: Some(Arc::new(store)),
            },
            Ok(None) => AuthSnapshot::Missing,
            Err(error) => AuthSnapshot::Invalid(AuthLoadError::from_io(&error)),
        },
        Ok(None) => AuthSnapshot::Missing,
        Err(error) => AuthSnapshot::Invalid(AuthLoadError::from_io(&error)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionAuthKind {
    Configured,
    Missing,
    Invalid,
    ServerDeleted,
}

enum SessionAuthState {
    Configured {
        current: Arc<Credential>,
        revision: u64,
    },
    Missing,
    Invalid(AuthLoadError),
    ServerDeleted {
        revision: u64,
    },
}

impl fmt::Debug for SessionAuthState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configured { revision, .. } => formatter
                .debug_struct("Configured")
                .field("credential", &"<redacted>")
                .field("revision", revision)
                .finish(),
            Self::Missing => formatter.write_str("Missing"),
            Self::Invalid(error) => formatter.debug_tuple("Invalid").field(error).finish(),
            Self::ServerDeleted { revision } => formatter
                .debug_struct("ServerDeleted")
                .field("revision", revision)
                .finish(),
        }
    }
}

#[derive(Clone)]
struct SessionAuthToken {
    revision: u64,
    expected: Arc<Credential>,
}

impl fmt::Debug for SessionAuthToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionAuthToken")
            .field("revision", &self.revision)
            .field("expected", &"<redacted>")
            .finish()
    }
}

enum SessionMutation {
    Replace(Arc<Credential>),
    Delete,
}

enum PendingPersistence {
    Replace {
        expected: Arc<Credential>,
        new: Arc<Credential>,
    },
    Remove {
        expected: Arc<Credential>,
    },
}

impl fmt::Debug for PendingPersistence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Replace { .. } => "Replace(<redacted>)",
            Self::Remove { .. } => "Remove(<redacted>)",
        })
    }
}

struct SessionAuthInner {
    state: SessionAuthState,
    pending: Vec<PendingPersistence>,
}

pub(crate) struct SessionAuth {
    inner: Mutex<SessionAuthInner>,
    exchange: Mutex<()>,
    store: Option<Arc<AuthStore>>,
    bootstrap: Option<CredentialIdentity>,
}

impl fmt::Debug for SessionAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        formatter
            .debug_struct("SessionAuth")
            .field("state", &inner.state)
            .field("pending", &inner.pending.len())
            .finish()
    }
}

impl SessionAuth {
    pub(crate) fn from_snapshot(snapshot: &AuthSnapshot) -> Arc<Self> {
        let state = match snapshot {
            AuthSnapshot::Configured { credential, .. } => SessionAuthState::Configured {
                current: Arc::clone(credential),
                revision: 0,
            },
            AuthSnapshot::Missing => SessionAuthState::Missing,
            AuthSnapshot::Invalid(error) => SessionAuthState::Invalid(*error),
        };
        Arc::new(Self {
            inner: Mutex::new(SessionAuthInner {
                state,
                pending: Vec::new(),
            }),
            exchange: Mutex::new(()),
            store: snapshot.store(),
            bootstrap: snapshot.credential_identity(),
        })
    }

    pub(crate) fn authentication_lineage(&self) -> Option<AuthenticationLineage> {
        let bootstrap = self.bootstrap.clone()?;
        let runtime = match &self.lock_inner().state {
            SessionAuthState::Configured { current, .. } => {
                AuthenticationRuntimeIdentity::Configured(CredentialIdentity(Arc::clone(current)))
            }
            SessionAuthState::ServerDeleted { .. } => AuthenticationRuntimeIdentity::ServerDeleted,
            SessionAuthState::Missing | SessionAuthState::Invalid(_) => return None,
        };
        Some(AuthenticationLineage { bootstrap, runtime })
    }

    #[cfg(test)]
    pub(crate) fn configured_for_test(value: &str) -> Arc<Self> {
        Self::from_snapshot(&AuthSnapshot::configured_for_test(value))
    }

    #[cfg(test)]
    pub(crate) fn load_from_location_for_test(location: &CookieLocation) -> Arc<Self> {
        Self::from_snapshot(&load_auth_snapshot_from(location))
    }

    pub(crate) fn kind(&self) -> SessionAuthKind {
        match &self.lock_inner().state {
            SessionAuthState::Configured { .. } => SessionAuthKind::Configured,
            SessionAuthState::Missing => SessionAuthKind::Missing,
            SessionAuthState::Invalid(_) => SessionAuthKind::Invalid,
            SessionAuthState::ServerDeleted { .. } => SessionAuthKind::ServerDeleted,
        }
    }

    pub(crate) fn submission_unavailable_message(&self) -> Option<&'static str> {
        match self.kind() {
            SessionAuthKind::Configured => None,
            SessionAuthKind::Missing => {
                Some("Authentication is not configured.\nReturn Home to configure authentication.")
            }
            SessionAuthKind::Invalid => {
                Some("Authentication configuration is invalid.\nReturn Home to fix authentication.")
            }
            SessionAuthKind::ServerDeleted => Some(
                "Authentication was removed by AtCoder.\nReturn Home to configure authentication.",
            ),
        }
    }

    pub(crate) fn cookie_header(&self) -> Option<reqwest::header::HeaderValue> {
        let inner = self.lock_inner();
        let SessionAuthState::Configured { current, .. } = &inner.state else {
            return None;
        };
        let mut header = reqwest::header::HeaderValue::from_str(current.as_str()).ok()?;
        header.set_sensitive(true);
        Some(header)
    }

    pub(crate) fn with_exchange<T>(&self, exchange: impl FnOnce() -> T) -> T {
        let _guard = self
            .exchange
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        exchange()
    }

    #[cfg(test)]
    pub(crate) fn observe_set_cookie_batch<'a>(
        &self,
        headers: impl Iterator<Item = &'a reqwest::header::HeaderValue>,
        response_url: &reqwest::Url,
    ) {
        if !is_exact_atcoder_origin(response_url) {
            return;
        }
        self.observe_trusted_set_cookie_batch(headers, response_url);
    }

    pub(crate) fn observe_trusted_set_cookie_batch<'a>(
        &self,
        headers: impl Iterator<Item = &'a reqwest::header::HeaderValue>,
        response_url: &reqwest::Url,
    ) {
        let Some(token) = self.current_token() else {
            return;
        };
        let mut final_mutation = None;
        for header in headers {
            let Ok(text) = header.to_str() else {
                continue;
            };
            let Ok(cookie) = Cookie::parse(text) else {
                continue;
            };
            if cookie.name() != "REVEL_SESSION" || !accepted_cookie_scope(&cookie, response_url) {
                continue;
            }
            if cookie_is_deletion(&cookie) {
                final_mutation = Some(SessionMutation::Delete);
                continue;
            }
            if let Ok(credential) = Credential::from_cookie_value(cookie.value()) {
                final_mutation = Some(SessionMutation::Replace(Arc::new(credential)));
            }
        }
        if let Some(mutation) = final_mutation {
            self.update_if_current(&token, mutation);
        }
    }

    pub(crate) fn persist_pending(&self) -> Vec<AuthPersistenceWarning> {
        let pending = {
            let mut inner = self.lock_inner();
            std::mem::take(&mut inner.pending)
        };
        let Some(store) = self.store.as_ref() else {
            return Vec::new();
        };
        pending
            .into_iter()
            .filter_map(|mutation| {
                let result = match mutation {
                    PendingPersistence::Replace { expected, new } => {
                        store.replace_if_current(&expected, &new)
                    }
                    PendingPersistence::Remove { expected } => store.remove_if_current(&expected),
                };
                match result {
                    Ok(AuthStoreMutationOutcome::Applied)
                    | Ok(AuthStoreMutationOutcome::AlreadyApplied) => None,
                    Ok(AuthStoreMutationOutcome::Conflict) => {
                        Some(AuthPersistenceWarning::Conflict)
                    }
                    Ok(AuthStoreMutationOutcome::Missing) => Some(AuthPersistenceWarning::Missing),
                    Err(error) => Some(AuthPersistenceWarning::from_io(&error)),
                }
            })
            .collect()
    }

    fn current_token(&self) -> Option<SessionAuthToken> {
        let inner = self.lock_inner();
        match &inner.state {
            SessionAuthState::Configured { current, revision } => Some(SessionAuthToken {
                revision: *revision,
                expected: Arc::clone(current),
            }),
            SessionAuthState::Missing
            | SessionAuthState::Invalid(_)
            | SessionAuthState::ServerDeleted { .. } => None,
        }
    }

    fn update_if_current(&self, token: &SessionAuthToken, mutation: SessionMutation) -> bool {
        let mut inner = self.lock_inner();
        let SessionAuthState::Configured { current, revision } = &inner.state else {
            return false;
        };
        if *revision != token.revision || !current.same_value(&token.expected) {
            return false;
        }
        let Some(next_revision) = revision.checked_add(1) else {
            return false;
        };
        let expected = Arc::clone(current);
        match mutation {
            SessionMutation::Replace(new) => {
                if current.same_value(&new) {
                    return true;
                }
                inner.state = SessionAuthState::Configured {
                    current: Arc::clone(&new),
                    revision: next_revision,
                };
                inner
                    .pending
                    .push(PendingPersistence::Replace { expected, new });
            }
            SessionMutation::Delete => {
                inner.state = SessionAuthState::ServerDeleted {
                    revision: next_revision,
                };
                inner.pending.push(PendingPersistence::Remove { expected });
            }
        }
        true
    }

    fn lock_inner(&self) -> MutexGuard<'_, SessionAuthInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(test)]
    pub(crate) fn credential_matches_for_test(&self, expected: &str) -> bool {
        let inner = self.lock_inner();
        matches!(&inner.state, SessionAuthState::Configured { current, .. } if current.as_str() == expected)
    }

    #[cfg(test)]
    fn revision_for_test(&self) -> Option<u64> {
        let inner = self.lock_inner();
        match inner.state {
            SessionAuthState::Configured { revision, .. }
            | SessionAuthState::ServerDeleted { revision } => Some(revision),
            SessionAuthState::Missing | SessionAuthState::Invalid(_) => None,
        }
    }
}

#[cfg(test)]
fn is_exact_atcoder_origin(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some("atcoder.jp")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
}

fn accepted_cookie_scope(cookie: &Cookie<'_>, response_url: &reqwest::Url) -> bool {
    let domain_ok = cookie.domain().is_none_or(|domain| {
        domain
            .trim_start_matches('.')
            .eq_ignore_ascii_case("atcoder.jp")
    });
    let path_ok = cookie.path().map_or_else(
        || response_default_cookie_path_is_root(response_url),
        |path| path == "/",
    );
    domain_ok && path_ok
}

fn response_default_cookie_path_is_root(response_url: &reqwest::Url) -> bool {
    let path = response_url.path();
    !path.starts_with('/') || !path[1..].contains('/')
}

fn cookie_is_deletion(cookie: &Cookie<'_>) -> bool {
    if cookie.value().is_empty() {
        return true;
    }
    if let Some(max_age) = cookie.max_age() {
        return max_age.whole_seconds() <= 0;
    }
    cookie
        .expires_datetime()
        .is_some_and(|expires| SystemTime::from(expires) <= SystemTime::now())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthPersistenceWarning {
    Conflict,
    Missing,
    Unsafe,
    Io,
}

impl AuthPersistenceWarning {
    fn from_io(error: &io::Error) -> Self {
        match AuthLoadError::from_io(error).kind {
            AuthLoadErrorKind::InvalidFormat
            | AuthLoadErrorKind::UnsafeFilesystemState
            | AuthLoadErrorKind::Permission => Self::Unsafe,
            AuthLoadErrorKind::Io => Self::Io,
        }
    }
}

impl fmt::Display for AuthPersistenceWarning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Conflict => {
                "AtCoder rotated authentication, but the external credential changed; the external value was preserved"
            }
            Self::Missing => {
                "AtCoder rotated authentication, but the external credential is missing; it was not recreated"
            }
            Self::Unsafe => {
                "AtCoder rotated authentication, but the external credential could not be updated safely"
            }
            Self::Io => {
                "AtCoder rotated authentication, but the external credential could not be persisted"
            }
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthStoreMutationOutcome {
    Applied,
    AlreadyApplied,
    Conflict,
    Missing,
}

pub(crate) struct AuthStore {
    directory: Dir,
}

impl fmt::Debug for AuthStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthStore(<pinned>)")
    }
}

impl AuthStore {
    fn open(location: &CookieLocation) -> io::Result<Option<Self>> {
        Ok(open_state_directory(location)?.map(|directory| Self { directory }))
    }

    fn open_or_create(location: &CookieLocation) -> io::Result<Self> {
        Ok(Self {
            directory: open_or_create_state_directory(location)?,
        })
    }

    fn load_current(&self) -> io::Result<Option<Credential>> {
        load_credential_from_directory(&self.directory)
    }

    pub(crate) fn replace_if_current(
        &self,
        expected: &Credential,
        new: &Credential,
    ) -> io::Result<AuthStoreMutationOutcome> {
        self.mutate_if_current(expected, Some(new), replace_cookie_file)
    }

    pub(crate) fn remove_if_current(
        &self,
        expected: &Credential,
    ) -> io::Result<AuthStoreMutationOutcome> {
        self.mutate_if_current(expected, None, |_, _| {
            unreachable!("remove_if_current does not replace a credential")
        })
    }

    #[cfg(test)]
    fn replace_if_current_with_hook(
        &self,
        expected: &Credential,
        new: &Credential,
        before_stage: impl FnMut(AuthStoreWriteStage, &Dir, Option<&str>) -> io::Result<()>,
    ) -> io::Result<AuthStoreMutationOutcome> {
        self.mutate_if_current(expected, Some(new), |directory, credential| {
            replace_cookie_file_with_hook(directory, credential, before_stage)
        })
    }

    fn mutate_if_current(
        &self,
        expected: &Credential,
        new: Option<&Credential>,
        replace: impl FnOnce(&Dir, &Credential) -> io::Result<()>,
    ) -> io::Result<AuthStoreMutationOutcome> {
        let _process_lock = PROCESS_AUTH_STORE_LOCK
            .lock()
            .map_err(|_| io::Error::other("process-local authentication storage lock poisoned"))?;
        let lock = open_auth_store_lock(&self.directory)?;
        lock.lock()
            .map_err(|_| io::Error::other("authentication storage lock failed"))?;
        let Some(current) = self.load_current()? else {
            return Ok(AuthStoreMutationOutcome::Missing);
        };
        if let Some(new) = new
            && current.same_value(new)
        {
            return Ok(AuthStoreMutationOutcome::AlreadyApplied);
        }
        if !current.same_value(expected) {
            return Ok(AuthStoreMutationOutcome::Conflict);
        }
        match new {
            Some(new) => replace(&self.directory, new)?,
            None => {
                self.directory.remove_file("cookie")?;
                sync_directory(&self.directory)?;
            }
        }
        Ok(AuthStoreMutationOutcome::Applied)
    }

    fn write_explicit(&self, credential: &Credential) -> io::Result<Credential> {
        self.write_explicit_with(credential, replace_cookie_file)
    }

    fn write_explicit_with(
        &self,
        credential: &Credential,
        replace: impl FnOnce(&Dir, &Credential) -> io::Result<()>,
    ) -> io::Result<Credential> {
        let _process_lock = PROCESS_AUTH_STORE_LOCK
            .lock()
            .map_err(|_| io::Error::other("process-local authentication storage lock poisoned"))?;
        let lock = open_auth_store_lock(&self.directory)?;
        lock.lock()
            .map_err(|_| io::Error::other("authentication storage lock failed"))?;
        validate_cookie_entry_for_explicit_mutation(&self.directory)?;
        replace(&self.directory, credential)?;
        let read_back = self.load_current()?.ok_or_else(|| {
            io::Error::other("authentication credential disappeared during locked read-back")
        })?;
        if !read_back.same_value(credential) {
            return Err(io::Error::other(
                "authentication credential changed during locked read-back",
            ));
        }
        Ok(read_back)
    }

    fn reset_explicit(&self) -> io::Result<ExplicitResetOutcome> {
        let _process_lock = PROCESS_AUTH_STORE_LOCK
            .lock()
            .map_err(|_| io::Error::other("process-local authentication storage lock poisoned"))?;
        let lock = open_auth_store_lock(&self.directory)?;
        lock.lock()
            .map_err(|_| io::Error::other("authentication storage lock failed"))?;
        match validate_cookie_entry_for_explicit_mutation(&self.directory)? {
            CookieFileState::Missing => Ok(ExplicitResetOutcome::AlreadyMissing),
            CookieFileState::Existing => {
                self.directory.remove_file("cookie")?;
                sync_directory(&self.directory)?;
                Ok(ExplicitResetOutcome::Removed)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExplicitResetOutcome {
    Removed,
    AlreadyMissing,
}

/// Store a credential supplied by an explicit user action, then read it back through the normal
/// parser and permission checks. Unlike the server CAS APIs, this operation intentionally replaces
/// any safe regular cookie entry even when the value is unchanged or malformed.
pub(crate) fn write_explicit_credential(
    location: &CookieLocation,
    credential: &Credential,
) -> io::Result<Arc<AuthSnapshot>> {
    validate_cookie_location(location)?;
    let store = Arc::new(AuthStore::open_or_create(location)?);
    let credential = store.write_explicit(credential)?;
    Ok(Arc::new(AuthSnapshot::Configured {
        credential: Arc::new(credential),
        store: Some(store),
    }))
}

/// Remove the cookie entry without parsing its contents. A wholly missing hierarchy remains
/// missing; no directory or lock file is created in that case.
pub(crate) fn reset_explicit_credential(
    location: &CookieLocation,
) -> io::Result<ExplicitResetOutcome> {
    validate_cookie_location(location)?;
    let Some(store) = AuthStore::open(location)? else {
        return Ok(ExplicitResetOutcome::AlreadyMissing);
    };
    store.reset_explicit()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CookieFileState {
    Missing,
    Existing,
}

/// Inspect cookie state without reading credential contents.
///
/// The file is opened with the same path-hierarchy, no-follow, regular-file, and permission checks
/// used by authentication. This is intended for status and setup guidance only: the result does
/// not make a later pathname reopen safe or guarantee that it would refer to the inspected object.
#[cfg(test)]
pub(crate) fn inspect_cookie_file(location: &CookieLocation) -> io::Result<CookieFileState> {
    match open_validated_cookie_file(location)? {
        Some(_) => Ok(CookieFileState::Existing),
        None => Ok(CookieFileState::Missing),
    }
}

#[cfg(test)]
fn load_credential_from(location: &CookieLocation) -> io::Result<Option<Credential>> {
    let Some(file) = open_validated_cookie_file(location)? else {
        return Ok(None);
    };

    let mut cookie = String::new();
    file.take((MAX_COOKIE_LINE_BYTES + 3) as u64)
        .read_to_string(&mut cookie)?;

    Ok(Some(parse_cookie_file(&cookie)?))
}

fn parse_cookie_file(contents: &str) -> io::Result<Credential> {
    if contents.len() > MAX_COOKIE_LINE_BYTES + 2 {
        return Err(invalid_cookie_file_error());
    }

    let cookie = contents
        .strip_suffix("\r\n")
        .or_else(|| contents.strip_suffix('\n'))
        .unwrap_or(contents);

    if cookie.len() > MAX_COOKIE_LINE_BYTES || cookie.contains('\r') || cookie.contains('\n') {
        return Err(invalid_cookie_file_error());
    }

    let Some(value) = cookie.strip_prefix(SESSION_COOKIE_PREFIX) else {
        return Err(invalid_cookie_file_error());
    };

    Credential::from_cookie_value(value)
}

fn is_cookie_octet(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'#'..=b'+' | b'-'..=b':' | b'<'..=b'[' | b']'..=b'~'
    )
}

fn invalid_cookie_file_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "authentication cookie must have the form REVEL_SESSION=<value>",
    )
}

fn validate_cookie_location(location: &CookieLocation) -> io::Result<()> {
    if !location.platform_base.is_absolute()
        || !location.state_dir.is_absolute()
        || !location.file.is_absolute()
        || location.file.parent() != Some(location.state_dir.as_path())
        || location.file.file_name() != Some(std::ffi::OsStr::new("cookie"))
    {
        return Err(unsafe_path_error("cookie location is invalid"));
    }

    Ok(())
}

#[cfg(test)]
fn open_cookie_file(location: &CookieLocation) -> io::Result<Option<fs::File>> {
    open_cookie_file_with(location, || {})
}

#[cfg(test)]
fn open_validated_cookie_file(location: &CookieLocation) -> io::Result<Option<fs::File>> {
    validate_cookie_location(location)?;
    let Some(file) = open_cookie_file(location)? else {
        return Ok(None);
    };

    if !file.metadata()?.file_type().is_file() {
        return Err(unsafe_path_error("cookie path is not a regular file"));
    }
    validate_cookie_file_permissions(&file)?;
    Ok(Some(file))
}

#[cfg(test)]
fn open_cookie_file_with(
    location: &CookieLocation,
    before_cookie_open: impl FnOnce(),
) -> io::Result<Option<fs::File>> {
    // Validate the complete lexical relationship before filesystem existence
    // can turn an invalid location into a misleading NotConfigured result.
    let Some(directory) = open_state_directory(location)? else {
        return Ok(None);
    };

    before_cookie_open();

    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);

    match directory.open_with("cookie", &options) {
        Ok(file) => Ok(Some(file.into_std())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn open_state_directory(location: &CookieLocation) -> io::Result<Option<Dir>> {
    let state_directories = application_state_directories(location)?;

    // The platform base may itself be redirected by the operating system, so
    // it is opened with ambient authority. Every atc-rs-owned descendant is
    // then opened relative to a pinned directory handle without following the
    // final component. Keeping the final handle in AuthStore also prevents a
    // later pathname swap from redirecting session persistence.
    let mut directory = match Dir::open_ambient_dir(&location.platform_base, ambient_authority()) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };

    for path in state_directories {
        let component = path
            .file_name()
            .ok_or_else(|| unsafe_path_error("cookie state path is invalid"))?;
        directory = match directory.open_dir_nofollow(component) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        validate_state_directory(&directory)?;
    }
    Ok(Some(directory))
}

fn open_or_create_state_directory(location: &CookieLocation) -> io::Result<Dir> {
    let state_directories = application_state_directories(location)?;

    match fs::create_dir_all(&location.platform_base) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let mut directory = Dir::open_ambient_dir(&location.platform_base, ambient_authority())?;

    for path in state_directories {
        let component = path
            .file_name()
            .ok_or_else(|| unsafe_path_error("cookie state path is invalid"))?;
        match directory.create_dir(component) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        directory = directory.open_dir_nofollow(component)?;
        validate_state_directory(&directory)?;
    }
    Ok(directory)
}

fn validate_cookie_entry_for_explicit_mutation(directory: &Dir) -> io::Result<CookieFileState> {
    match directory.symlink_metadata("cookie") {
        Ok(metadata) => {
            if metadata_is_reparse(&metadata) || !metadata.file_type().is_file() {
                return Err(unsafe_path_error("cookie path is not a regular file"));
            }
            Ok(CookieFileState::Existing)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(CookieFileState::Missing),
        Err(error) => Err(error),
    }
}

fn load_credential_from_directory(directory: &Dir) -> io::Result<Option<Credential>> {
    match directory.symlink_metadata("cookie") {
        Ok(metadata) => {
            if metadata_is_reparse(&metadata) || !metadata.file_type().is_file() {
                return Err(unsafe_path_error("cookie path is not a regular file"));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    let file = match directory.open_with("cookie", &options) {
        Ok(file) => file.into_std(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !file.metadata()?.file_type().is_file() {
        return Err(unsafe_path_error("cookie path is not a regular file"));
    }
    validate_cookie_file_permissions(&file)?;
    let mut cookie = String::new();
    file.take((MAX_COOKIE_LINE_BYTES + 3) as u64)
        .read_to_string(&mut cookie)?;
    Ok(Some(parse_cookie_file(&cookie)?))
}

fn open_auth_store_lock(directory: &Dir) -> io::Result<fs::File> {
    match directory.symlink_metadata(AUTH_LOCK_FILE) {
        Ok(metadata) => {
            if metadata_is_reparse(&metadata) || !metadata.file_type().is_file() {
                return Err(unsafe_path_error(
                    "authentication storage lock is not a regular file",
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .follow(FollowSymlinks::No)
        .nonblock(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = directory.open_with(AUTH_LOCK_FILE, &options)?;
    if metadata_is_reparse(&file.metadata()?) || !file.metadata()?.file_type().is_file() {
        return Err(unsafe_path_error(
            "authentication storage lock is not a regular file",
        ));
    }
    Ok(file.into_std())
}

struct StagedCredential<'a> {
    directory: &'a Dir,
    name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthStoreWriteStage {
    StagingCreate,
    Write,
    FileSync,
    Publish,
}

impl Drop for StagedCredential<'_> {
    fn drop(&mut self) {
        if !self.name.is_empty() {
            let _ = self.directory.remove_file(&self.name);
        }
    }
}

fn replace_cookie_file(directory: &Dir, credential: &Credential) -> io::Result<()> {
    replace_cookie_file_with_hook(directory, credential, |_, _, _| Ok(()))
}

fn replace_cookie_file_with_hook(
    directory: &Dir,
    credential: &Credential,
    mut before_stage: impl FnMut(AuthStoreWriteStage, &Dir, Option<&str>) -> io::Result<()>,
) -> io::Result<()> {
    before_stage(AuthStoreWriteStage::StagingCreate, directory, None)?;
    let mut staged = None;
    for _ in 0..128 {
        let sequence = NEXT_AUTH_STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let name = format!("{AUTH_STAGING_PREFIX}{}-{sequence}", std::process::id());
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No)
            .nonblock(true);
        #[cfg(unix)]
        options.mode(0o600);
        match directory.open_with(&name, &options) {
            Ok(file) => {
                staged = Some((StagedCredential { directory, name }, file.into_std()));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    let Some((mut staged, mut file)) = staged else {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate authentication staging file",
        ));
    };
    if !file.metadata()?.file_type().is_file() {
        return Err(unsafe_path_error(
            "authentication staging path is not a regular file",
        ));
    }
    #[cfg(unix)]
    validate_cookie_file_permissions(&file)?;
    before_stage(AuthStoreWriteStage::Write, directory, Some(&staged.name))?;
    file.write_all(credential.as_str().as_bytes())?;
    before_stage(AuthStoreWriteStage::FileSync, directory, Some(&staged.name))?;
    file.sync_all()?;
    drop(file);
    before_stage(AuthStoreWriteStage::Publish, directory, Some(&staged.name))?;
    directory.rename(&staged.name, directory, "cookie")?;
    staged.name.clear();
    sync_directory(directory)
}

#[cfg(unix)]
fn sync_directory(directory: &Dir) -> io::Result<()> {
    directory.try_clone()?.into_std_file().sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Dir) -> io::Result<()> {
    Ok(())
}

fn validate_state_directory(directory: &Dir) -> io::Result<()> {
    let metadata = directory.dir_metadata()?;
    if metadata_is_reparse(&metadata) || !metadata.file_type().is_dir() {
        return Err(unsafe_path_error(
            "authentication state path is not a real directory",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn metadata_is_reparse(metadata: &cap_std::fs::Metadata) -> bool {
    use cap_std::fs::MetadataExt as _;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse(_metadata: &cap_std::fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn validate_cookie_file_permissions(file: &fs::File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if file.metadata()?.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "authentication cookie must not be accessible by group or other users (use chmod 600)",
        ));
    }

    Ok(())
}

#[cfg(not(unix))]
fn validate_cookie_file_permissions(_file: &fs::File) -> io::Result<()> {
    Ok(())
}

fn application_state_directories(location: &CookieLocation) -> io::Result<Vec<PathBuf>> {
    let relative = location
        .state_dir
        .strip_prefix(&location.platform_base)
        .map_err(|_| unsafe_path_error("cookie state path is outside its platform base"))?;
    let mut current = location.platform_base.clone();
    let mut directories = Vec::new();

    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(unsafe_path_error("cookie state path is invalid"));
        };
        current.push(component);
        directories.push(current.clone());
    }

    if directories.last() != Some(&location.state_dir) {
        return Err(unsafe_path_error("cookie state path is invalid"));
    }

    Ok(directories)
}

fn unsafe_path_error(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::path::Path;

    fn location(root: &Path) -> CookieLocation {
        let platform_base = root.join("platform-state");
        let state_dir = platform_base.join("atc").join("state");
        let file = state_dir.join("cookie");
        CookieLocation {
            platform_base,
            state_dir,
            file,
        }
    }

    fn create_file_symlink(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        let result = std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        let result = std::os::windows::fs::symlink_file(target, link);

        match result {
            Ok(()) => true,
            #[cfg(windows)]
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(1314) =>
            {
                false
            }
            Err(error) => panic!("failed to create file symlink: {error}"),
        }
    }

    fn create_directory_symlink(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        let result = std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        let result = std::os::windows::fs::symlink_dir(target, link);

        match result {
            Ok(()) => true,
            #[cfg(windows)]
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(1314) =>
            {
                false
            }
            Err(error) => panic!("failed to create directory symlink: {error}"),
        }
    }

    fn write_cookie_file(path: &Path, contents: impl AsRef<[u8]>) {
        fs::write(path, contents).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn missing_cookie_is_the_only_anonymous_state() {
        let temp = tempfile::tempdir().unwrap();
        let location = location(temp.path());
        assert!(load_credential_from(&location).unwrap().is_none());
        assert!(matches!(
            load_auth_snapshot_from(&location),
            AuthSnapshot::Missing
        ));
        assert_eq!(
            inspect_cookie_file(&location).unwrap(),
            CookieFileState::Missing
        );

        fs::create_dir_all(&location.state_dir).unwrap();
        write_cookie_file(&location.file, " \r\n ");
        assert_eq!(
            load_credential_from(&location).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(matches!(
            load_auth_snapshot_from(&location),
            AuthSnapshot::Invalid(error)
                if error.kind() == AuthLoadErrorKind::InvalidFormat
        ));

        write_cookie_file(&location.file, [0xff]);
        assert_eq!(
            load_credential_from(&location).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(matches!(
            load_auth_snapshot_from(&location),
            AuthSnapshot::Invalid(error)
                if error.kind() == AuthLoadErrorKind::InvalidFormat
        ));

        write_cookie_file(&location.file, "value-only");
        assert_eq!(
            load_credential_from(&location).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        write_cookie_file(&location.file, "REVEL_SESSION=secret\n");
        assert!(load_credential_from(&location).is_ok_and(|credential| {
            credential.is_some_and(|credential| credential.as_str() == "REVEL_SESSION=secret")
        }));
        assert_eq!(
            inspect_cookie_file(&location).unwrap(),
            CookieFileState::Existing
        );
        assert!(matches!(
            load_auth_snapshot_from(&location),
            AuthSnapshot::Configured { .. }
        ));
    }

    #[test]
    fn credential_and_auth_snapshot_debug_are_redacted() {
        let marker = "REVEL_SESSION=distinctive-auth-secret-7f2c";
        let snapshot = AuthSnapshot::Configured {
            credential: Arc::new(Credential {
                value: marker.to_string(),
            }),
            store: None,
        };
        let credential_debug = format!("{:?}", snapshot.credential().unwrap());
        let snapshot_debug = format!("{snapshot:?}");

        assert_eq!(credential_debug, "Credential(<redacted>)");
        assert!(!credential_debug.contains(marker));
        assert!(!snapshot_debug.contains(marker));
        assert!(snapshot_debug.contains("Configured"));

        let invalid = AuthSnapshot::Invalid(AuthLoadError {
            kind: AuthLoadErrorKind::InvalidFormat,
        });
        assert!(!format!("{invalid:?}").contains(marker));
        assert!(
            !invalid
                .submission_unavailable_message()
                .unwrap()
                .contains(marker)
        );
    }

    #[test]
    fn relative_cookie_location_is_rejected() {
        let location = CookieLocation {
            platform_base: PathBuf::from("relative-platform-state"),
            state_dir: PathBuf::from("relative-platform-state/atc/state"),
            file: PathBuf::from("relative-platform-state/atc/state/cookie"),
        };

        assert!(load_credential_from(&location).is_err());
    }

    #[test]
    fn cookie_location_must_stay_beneath_its_platform_base() {
        let temp = tempfile::tempdir().unwrap();
        let platform_base = temp.path().join("platform-state");

        for state_dir in [
            temp.path().join("outside-state"),
            platform_base.clone(),
            platform_base.join("atc").join("..").join("state"),
        ] {
            let location = CookieLocation {
                platform_base: platform_base.clone(),
                file: state_dir.join("cookie"),
                state_dir,
            };

            assert!(load_credential_from(&location).is_err());
            assert!(inspect_cookie_file(&location).is_err());
        }
    }

    #[test]
    fn cookie_symlink_and_directory_are_rejected_without_touching_the_target() {
        let symlink_root = tempfile::tempdir().unwrap();
        let external = tempfile::NamedTempFile::new().unwrap();
        fs::write(external.path(), "external secret").unwrap();
        let symlink_location = location(symlink_root.path());
        fs::create_dir_all(&symlink_location.state_dir).unwrap();
        if !create_file_symlink(external.path(), &symlink_location.file) {
            return;
        }

        assert!(load_credential_from(&symlink_location).is_err());
        assert!(matches!(
            load_auth_snapshot_from(&symlink_location),
            AuthSnapshot::Invalid(_)
        ));
        assert!(inspect_cookie_file(&symlink_location).is_err());
        assert_eq!(
            fs::read_to_string(external.path()).unwrap(),
            "external secret"
        );

        let directory_root = tempfile::tempdir().unwrap();
        let directory_location = location(directory_root.path());
        fs::create_dir_all(&directory_location.file).unwrap();
        assert!(load_credential_from(&directory_location).is_err());
        assert!(inspect_cookie_file(&directory_location).is_err());
    }

    #[test]
    fn symlinked_application_state_directory_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let location = location(temp.path());
        fs::create_dir_all(location.state_dir.parent().unwrap()).unwrap();
        if !create_directory_symlink(external.path(), &location.state_dir) {
            return;
        }

        assert!(load_credential_from(&location).is_err());
        assert!(inspect_cookie_file(&location).is_err());
        assert!(!external.path().join("cookie").exists());
    }

    #[cfg(unix)]
    #[test]
    fn unix_socket_cookie_is_rejected_as_a_special_file() {
        use std::os::unix::net::UnixListener;

        let temp = tempfile::tempdir().unwrap();
        let location = location(temp.path());
        fs::create_dir_all(&location.state_dir).unwrap();
        let _listener = UnixListener::bind(&location.file).unwrap();

        assert!(load_credential_from(&location).is_err());
        assert!(inspect_cookie_file(&location).is_err());
    }

    #[test]
    fn cookie_value_can_contain_percent() {
        assert!(
            parse_cookie_file("REVEL_SESSION=secret%value")
                .is_ok_and(|credential| credential.as_str() == "REVEL_SESSION=secret%value")
        );
    }

    #[test]
    fn cookie_file_requires_exact_revel_session_format() {
        assert!(
            parse_cookie_file("REVEL_SESSION=secret")
                .is_ok_and(|credential| credential.as_str() == "REVEL_SESSION=secret")
        );

        assert!(
            parse_cookie_file("REVEL_SESSION=secret\n")
                .is_ok_and(|credential| credential.as_str() == "REVEL_SESSION=secret")
        );

        assert!(
            parse_cookie_file("REVEL_SESSION=secret\r\n")
                .is_ok_and(|credential| credential.as_str() == "REVEL_SESSION=secret")
        );

        for invalid in [
            "",
            "secret",
            "REVEL_SESSION=",
            "OTHER_COOKIE=secret",
            "REVEL_SESSION=secret\r",
            "REVEL_SESSION=secret\n\n",
            "REVEL_SESSION=secret\r\n\r\n",
            "REVEL_SESSION=secret\nOTHER_COOKIE=value",
            "REVEL_SESSION=secret; OTHER_COOKIE=value",
            "REVEL_SESSION=secret value",
            "REVEL_SESSION=secret\tvalue",
            "REVEL_SESSION=secret\0value",
            "REVEL_SESSION=secret\u{7f}value",
            "REVEL_SESSION=secret,OTHER_COOKIE=value",
            "REVEL_SESSION=\"secret\"",
            "REVEL_SESSION=secret\\value",
            "REVEL_SESSION=sécret",
        ] {
            assert_eq!(
                parse_cookie_file(invalid).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn pasted_credential_accepts_only_a_bare_or_single_prefixed_value() {
        for accepted in [
            "paste-value",
            "REVEL_SESSION=paste-value",
            "paste-value\n",
            "REVEL_SESSION=paste-value\r\n",
        ] {
            let credential = parse_pasted_credential(accepted).unwrap();
            assert_eq!(credential.as_str(), "REVEL_SESSION=paste-value");
        }

        let oversized = "x".repeat(MAX_COOKIE_LINE_BYTES);
        for rejected in [
            "".to_string(),
            "REVEL_SESSION=".to_string(),
            "REVEL_SESSION=REVEL_SESSION=nested".to_string(),
            "Cookie: REVEL_SESSION=value".to_string(),
            "Cookie:REVEL_SESSION=value".to_string(),
            "cookie:REVEL_SESSION=value".to_string(),
            "REVEL_SESSION=value; Path=/; HttpOnly".to_string(),
            "REVEL_SESSION=value; OTHER=value".to_string(),
            " value".to_string(),
            "value ".to_string(),
            "value\ninside".to_string(),
            "value\n\n".to_string(),
            "value\r\n\r\n".to_string(),
            "bad value".to_string(),
            "non-ascii-é".to_string(),
            oversized,
        ] {
            let error = parse_pasted_credential(&rejected).unwrap_err();
            let debug = format!("{error:?}");
            if !rejected.is_empty() {
                assert!(!debug.contains(&rejected));
            }
        }
    }

    #[test]
    fn cookie_file_size_is_bounded() {
        let max_value = "x".repeat(MAX_COOKIE_LINE_BYTES - SESSION_COOKIE_PREFIX.len());
        let max_cookie = format!("{SESSION_COOKIE_PREFIX}{max_value}");

        assert!(
            parse_cookie_file(&max_cookie)
                .is_ok_and(|credential| credential.as_str() == max_cookie)
        );
        assert!(parse_cookie_file(&format!("{max_cookie}\r\n")).is_ok());
        assert_eq!(
            parse_cookie_file(&format!("{max_cookie}x"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        let temp = tempfile::tempdir().unwrap();
        let location = location(temp.path());
        fs::create_dir_all(&location.state_dir).unwrap();
        write_cookie_file(&location.file, format!("{max_cookie}x"));
        assert_eq!(
            load_credential_from(&location).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn cookie_symlink_swap_immediately_before_open_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let external = tempfile::NamedTempFile::new().unwrap();
        let location = location(temp.path());
        fs::create_dir_all(&location.state_dir).unwrap();
        write_cookie_file(&location.file, "REVEL_SESSION=original");
        write_cookie_file(external.path(), "REVEL_SESSION=external");
        let swapped = Cell::new(false);

        let result = open_cookie_file_with(&location, || {
            fs::remove_file(&location.file).unwrap();
            swapped.set(create_file_symlink(external.path(), &location.file));
        });

        if !swapped.get() {
            return;
        }
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn opened_state_directory_is_not_redirected_by_a_later_symlink_swap() {
        let temp = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let location = location(temp.path());
        fs::create_dir_all(&location.state_dir).unwrap();
        write_cookie_file(&location.file, "REVEL_SESSION=original");
        write_cookie_file(&external.path().join("cookie"), "REVEL_SESSION=external");
        let moved_state = location.state_dir.with_file_name("original-state");

        let mut file = open_cookie_file_with(&location, || {
            fs::rename(&location.state_dir, &moved_state).unwrap();
            assert!(create_directory_symlink(
                external.path(),
                &location.state_dir
            ));
        })
        .unwrap()
        .unwrap();
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();

        assert!(contents == "REVEL_SESSION=original");
    }

    #[cfg(unix)]
    #[test]
    fn group_or_other_readable_cookie_is_rejected() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let location = location(temp.path());
        fs::create_dir_all(&location.state_dir).unwrap();
        fs::write(&location.file, "REVEL_SESSION=secret").unwrap();
        fs::set_permissions(&location.file, fs::Permissions::from_mode(0o644)).unwrap();

        assert_eq!(
            load_credential_from(&location).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(matches!(
            load_auth_snapshot_from(&location),
            AuthSnapshot::Invalid(error) if error.kind() == AuthLoadErrorKind::Permission
        ));
        assert_eq!(
            inspect_cookie_file(&location).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    fn observe(session: &SessionAuth, headers: &[&str], url: &str) {
        let headers = headers
            .iter()
            .map(|value| reqwest::header::HeaderValue::from_str(value).unwrap())
            .collect::<Vec<_>>();
        session.observe_set_cookie_batch(
            headers.iter(),
            &reqwest::Url::parse(url).expect("valid test URL"),
        );
    }

    fn managed_session(root: &Path, value: &str) -> (CookieLocation, Arc<SessionAuth>) {
        let location = location(root);
        fs::create_dir_all(&location.state_dir).unwrap();
        write_cookie_file(&location.file, value);
        let snapshot = load_auth_snapshot_from(&location);
        assert!(matches!(snapshot, AuthSnapshot::Configured { .. }));
        let session = SessionAuth::from_snapshot(&snapshot);
        (location, session)
    }

    fn assert_no_auth_staging_files(state_dir: &Path) {
        assert!(
            fs::read_dir(state_dir).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(AUTH_STAGING_PREFIX)
            }),
            "an authentication staging file was leaked"
        );
    }

    #[test]
    fn ordered_delete_then_successor_is_one_configured_transition_and_persists_final_value() {
        let temp = tempfile::tempdir().unwrap();
        let (location, session) = managed_session(temp.path(), "REVEL_SESSION=ordered-a");

        observe(
            &session,
            &[
                "REVEL_SESSION=; Path=/; HttpOnly; Secure",
                "REVEL_SESSION=ordered-b; Path=/; Max-Age=15552000; HttpOnly; Secure",
            ],
            "https://atcoder.jp/contests/abc500/tasks",
        );

        assert!(session.credential_matches_for_test("REVEL_SESSION=ordered-b"));
        assert_eq!(session.kind(), SessionAuthKind::Configured);
        assert_eq!(session.revision_for_test(), Some(1));
        assert!(session.persist_pending().is_empty());
        assert!(
            fs::read_to_string(location.file)
                .unwrap()
                .starts_with("REVEL_SESSION=ordered-b")
        );
    }

    #[test]
    fn session_evolution_does_not_mutate_the_bootstrap_snapshot() {
        let snapshot = AuthSnapshot::configured_for_test("REVEL_SESSION=snapshot-a");
        let session = SessionAuth::from_snapshot(&snapshot);

        observe(
            &session,
            &["REVEL_SESSION=session-b; Path=/"],
            "https://atcoder.jp/settings",
        );

        assert!(
            snapshot
                .credential()
                .is_some_and(|credential| credential.as_str() == "REVEL_SESSION=snapshot-a")
        );
        assert!(session.credential_matches_for_test("REVEL_SESSION=session-b"));
    }

    #[test]
    fn reverse_order_and_only_deletion_publish_server_deleted_without_cookie() {
        for headers in [
            vec!["REVEL_SESSION=reverse-b; Path=/", "REVEL_SESSION=; Path=/"],
            vec!["REVEL_SESSION=still-present; Path=/; Max-Age=0"],
            vec!["REVEL_SESSION=still-present; Path=/; Expires=Thu, 01 Jan 1970 00:00:00 GMT"],
        ] {
            let temp = tempfile::tempdir().unwrap();
            let (location, session) = managed_session(temp.path(), "REVEL_SESSION=delete-a");
            observe(&session, &headers, "https://atcoder.jp/settings");

            assert_eq!(session.kind(), SessionAuthKind::ServerDeleted);
            assert!(session.cookie_header().is_none());
            assert!(session.submission_unavailable_message().is_some());
            assert!(session.persist_pending().is_empty());
            assert!(!location.file.exists());
        }
    }

    #[test]
    fn max_age_takes_precedence_over_expires_for_deletion() {
        for (header, successor) in [
            (
                "REVEL_SESSION=positive-wins; Path=/; Max-Age=3600; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
                Some("REVEL_SESSION=positive-wins"),
            ),
            (
                "REVEL_SESSION=zero-wins; Path=/; Max-Age=0; Expires=Thu, 01 Jan 2099 00:00:00 GMT",
                None,
            ),
            (
                "REVEL_SESSION=negative-wins; Path=/; Max-Age=-1; Expires=Thu, 01 Jan 2099 00:00:00 GMT",
                None,
            ),
            (
                "REVEL_SESSION=past-expires; Path=/; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
                None,
            ),
            (
                "REVEL_SESSION=future-expires; Path=/; Expires=Thu, 01 Jan 2099 00:00:00 GMT",
                Some("REVEL_SESSION=future-expires"),
            ),
        ] {
            let session = SessionAuth::configured_for_test("REVEL_SESSION=precedence-a");
            observe(&session, &[header], "https://atcoder.jp/settings");
            match successor {
                Some(successor) => {
                    assert_eq!(session.kind(), SessionAuthKind::Configured);
                    assert!(session.credential_matches_for_test(successor));
                }
                None => {
                    assert_eq!(session.kind(), SessionAuthKind::ServerDeleted);
                    assert!(session.cookie_header().is_none());
                }
            }
        }
    }

    #[test]
    fn positive_max_age_successor_wins_after_ordered_deletion_even_with_past_expires() {
        let session = SessionAuth::configured_for_test("REVEL_SESSION=batch-precedence-a");

        observe(
            &session,
            &[
                "REVEL_SESSION=; Path=/",
                "REVEL_SESSION=batch-precedence-b; Path=/; Max-Age=3600; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
            ],
            "https://atcoder.jp/settings",
        );

        assert_eq!(session.kind(), SessionAuthKind::Configured);
        assert!(session.credential_matches_for_test("REVEL_SESSION=batch-precedence-b"));
        assert_eq!(session.revision_for_test(), Some(1));
    }

    #[test]
    fn missing_invalid_and_server_deleted_never_bootstrap_from_responses() {
        let invalid = AuthSnapshot::Invalid(AuthLoadError::from_io(&io::Error::new(
            io::ErrorKind::InvalidData,
            "redacted test reason",
        )));
        for snapshot in [&AuthSnapshot::Missing, &invalid] {
            let session = SessionAuth::from_snapshot(snapshot);
            let initial = session.kind();
            observe(
                &session,
                &["REVEL_SESSION=must-not-bootstrap; Path=/"],
                "https://atcoder.jp/",
            );
            assert_eq!(session.kind(), initial);
            assert!(session.cookie_header().is_none());
            assert!(session.persist_pending().is_empty());
        }

        let session = SessionAuth::configured_for_test("REVEL_SESSION=deleted-a");
        observe(&session, &["REVEL_SESSION=; Path=/"], "https://atcoder.jp/");
        observe(
            &session,
            &["REVEL_SESSION=must-not-recover; Path=/"],
            "https://atcoder.jp/",
        );
        assert_eq!(session.kind(), SessionAuthKind::ServerDeleted);
        assert!(session.cookie_header().is_none());
    }

    #[test]
    fn irrelevant_untrusted_and_incompatible_cookie_mutations_are_ignored() {
        let session = SessionAuth::configured_for_test("REVEL_SESSION=scope-a");
        for (header, url) in [
            ("REVEL_FLASH=flash; Path=/", "https://atcoder.jp/"),
            ("REVEL_SESSION=foreign; Path=/", "https://example.com/"),
            ("REVEL_SESSION=http; Path=/", "http://atcoder.jp/"),
            (
                "REVEL_SESSION=bad-domain; Domain=example.com; Path=/",
                "https://atcoder.jp/",
            ),
            (
                "REVEL_SESSION=bad-path; Path=/contests/abc500",
                "https://atcoder.jp/",
            ),
            (
                "REVEL_SESSION=implicit-nested-path",
                "https://atcoder.jp/contests/abc500/tasks",
            ),
            ("REVEL_SESSION=bad value; Path=/", "https://atcoder.jp/"),
        ] {
            observe(&session, &[header], url);
        }
        assert!(session.credential_matches_for_test("REVEL_SESSION=scope-a"));
        assert_eq!(session.revision_for_test(), Some(0));
    }

    #[test]
    fn root_default_path_is_compatible_when_path_is_omitted() {
        let session = SessionAuth::configured_for_test("REVEL_SESSION=default-path-a");

        observe(
            &session,
            &["REVEL_SESSION=default-path-b"],
            "https://atcoder.jp/settings",
        );

        assert!(session.credential_matches_for_test("REVEL_SESSION=default-path-b"));
    }

    #[test]
    fn revision_token_rejects_stale_and_aba_updates() {
        let session = SessionAuth::configured_for_test("REVEL_SESSION=aba-a");
        let stale_a = session.current_token().unwrap();
        assert!(session.update_if_current(
            &stale_a,
            SessionMutation::Replace(Arc::new(Credential::from_cookie_value("aba-b").unwrap()))
        ));
        let token_b = session.current_token().unwrap();
        assert!(session.update_if_current(
            &token_b,
            SessionMutation::Replace(Arc::new(Credential::from_cookie_value("aba-a").unwrap()))
        ));
        assert!(!session.update_if_current(
            &stale_a,
            SessionMutation::Replace(Arc::new(Credential::from_cookie_value("stale-c").unwrap()))
        ));
        assert!(session.credential_matches_for_test("REVEL_SESSION=aba-a"));
        assert_eq!(session.revision_for_test(), Some(2));
    }

    #[test]
    fn cross_process_change_wins_disk_cas_while_runtime_follows_server() {
        let temp = tempfile::tempdir().unwrap();
        let (location, session) = managed_session(temp.path(), "REVEL_SESSION=process-a");
        write_cookie_file(&location.file, "REVEL_SESSION=external-x");

        observe(
            &session,
            &["REVEL_SESSION=runtime-b; Path=/"],
            "https://atcoder.jp/",
        );
        let warnings = session.persist_pending();

        assert!(session.credential_matches_for_test("REVEL_SESSION=runtime-b"));
        assert_eq!(warnings, [AuthPersistenceWarning::Conflict]);
        assert!(
            fs::read_to_string(location.file)
                .unwrap()
                .starts_with("REVEL_SESSION=external-x")
        );
    }

    #[test]
    fn server_deletion_does_not_remove_an_external_successor() {
        let temp = tempfile::tempdir().unwrap();
        let (location, session) = managed_session(temp.path(), "REVEL_SESSION=delete-process-a");
        write_cookie_file(&location.file, "REVEL_SESSION=external-delete-x");

        observe(
            &session,
            &["REVEL_SESSION=; Path=/; Max-Age=0"],
            "https://atcoder.jp/settings",
        );
        let warnings = session.persist_pending();

        assert_eq!(session.kind(), SessionAuthKind::ServerDeleted);
        assert!(session.cookie_header().is_none());
        assert_eq!(warnings, [AuthPersistenceWarning::Conflict]);
        assert!(
            fs::read_to_string(location.file)
                .unwrap()
                .starts_with("REVEL_SESSION=external-delete-x")
        );
    }

    #[test]
    fn persistence_failure_is_nonfatal_redacted_and_keeps_runtime_successor() {
        let temp = tempfile::tempdir().unwrap();
        let (location, session) = managed_session(temp.path(), "REVEL_SESSION=failure-a");
        fs::create_dir(location.state_dir.join(AUTH_LOCK_FILE)).unwrap();

        observe(
            &session,
            &["REVEL_SESSION=failure-b; Path=/"],
            "https://atcoder.jp/",
        );
        let warnings = session.persist_pending();

        assert!(session.credential_matches_for_test("REVEL_SESSION=failure-b"));
        assert_eq!(warnings, [AuthPersistenceWarning::Unsafe]);
        let rendered = warnings[0].to_string();
        assert!(!rendered.contains("failure-a"));
        assert!(!rendered.contains("failure-b"));
        assert!(
            fs::read_to_string(location.file)
                .unwrap()
                .starts_with("REVEL_SESSION=failure-a")
        );
    }

    #[test]
    fn auth_store_replace_remove_and_outcomes_are_value_only_cas() {
        let temp = tempfile::tempdir().unwrap();
        let (location, _) = managed_session(temp.path(), "REVEL_SESSION=store-a");
        let store = AuthStore::open(&location).unwrap().unwrap();
        let a = Credential::from_cookie_value("store-a").unwrap();
        let b = Credential::from_cookie_value("store-b").unwrap();
        let c = Credential::from_cookie_value("store-c").unwrap();

        assert_eq!(
            store.replace_if_current(&a, &b).unwrap(),
            AuthStoreMutationOutcome::Applied
        );
        assert_eq!(
            store.replace_if_current(&a, &b).unwrap(),
            AuthStoreMutationOutcome::AlreadyApplied
        );
        assert_eq!(
            store.replace_if_current(&a, &c).unwrap(),
            AuthStoreMutationOutcome::Conflict
        );
        assert_eq!(
            store.remove_if_current(&a).unwrap(),
            AuthStoreMutationOutcome::Conflict
        );
        assert_eq!(
            store.remove_if_current(&b).unwrap(),
            AuthStoreMutationOutcome::Applied
        );
        assert_eq!(
            store.remove_if_current(&b).unwrap(),
            AuthStoreMutationOutcome::Missing
        );
        assert!(!location.file.exists());
    }

    #[test]
    fn auth_store_faults_before_staging_write_sync_and_publish_preserve_existing_credential() {
        for injected in [
            AuthStoreWriteStage::StagingCreate,
            AuthStoreWriteStage::Write,
            AuthStoreWriteStage::FileSync,
            AuthStoreWriteStage::Publish,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let (location, _) = managed_session(temp.path(), "REVEL_SESSION=fault-a");
            let store = AuthStore::open(&location).unwrap().unwrap();
            let expected = Credential::from_cookie_value("fault-a").unwrap();
            let replacement = Credential::from_cookie_value("fault-b").unwrap();

            let error = store
                .replace_if_current_with_hook(
                    &expected,
                    &replacement,
                    move |stage, _directory, _staged_name| {
                        if stage == injected {
                            Err(io::Error::other("injected authentication store failure"))
                        } else {
                            Ok(())
                        }
                    },
                )
                .unwrap_err();

            assert_eq!(error.kind(), io::ErrorKind::Other);
            assert!(
                fs::read_to_string(&location.file)
                    .unwrap()
                    .starts_with("REVEL_SESSION=fault-a")
            );
            assert_no_auth_staging_files(&location.state_dir);
        }
    }

    #[test]
    fn concurrent_auth_store_writers_have_one_cas_winner_and_one_conflict() {
        let temp = tempfile::tempdir().unwrap();
        let (location, _) = managed_session(temp.path(), "REVEL_SESSION=concurrent-a");
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut writers = Vec::new();
        for successor in ["concurrent-b", "concurrent-c"] {
            let location = location.clone();
            let barrier = Arc::clone(&barrier);
            writers.push(std::thread::spawn(move || {
                let store = AuthStore::open(&location).unwrap().unwrap();
                let expected = Credential::from_cookie_value("concurrent-a").unwrap();
                let successor = Credential::from_cookie_value(successor).unwrap();
                barrier.wait();
                store.replace_if_current(&expected, &successor).unwrap()
            }));
        }

        barrier.wait();
        let outcomes = writers
            .into_iter()
            .map(|writer| writer.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == AuthStoreMutationOutcome::Applied)
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == AuthStoreMutationOutcome::Conflict)
                .count(),
            1
        );
        let final_cookie = fs::read_to_string(&location.file).unwrap();
        assert!(
            final_cookie.starts_with("REVEL_SESSION=concurrent-b")
                || final_cookie.starts_with("REVEL_SESSION=concurrent-c")
        );
        assert!(location.state_dir.join(AUTH_LOCK_FILE).is_file());
        assert_no_auth_staging_files(&location.state_dir);
        assert!(load_credential_from(&location).unwrap().is_some());
    }

    #[test]
    fn auth_store_rejects_unsafe_replacement_without_touching_external_target() {
        let temp = tempfile::tempdir().unwrap();
        let external = tempfile::NamedTempFile::new().unwrap();
        write_cookie_file(external.path(), "REVEL_SESSION=external-safe");
        let (location, session) = managed_session(temp.path(), "REVEL_SESSION=unsafe-a");
        fs::remove_file(&location.file).unwrap();
        if !create_file_symlink(external.path(), &location.file) {
            return;
        }

        observe(
            &session,
            &["REVEL_SESSION=unsafe-b; Path=/"],
            "https://atcoder.jp/",
        );
        assert_eq!(session.persist_pending(), [AuthPersistenceWarning::Unsafe]);
        assert!(session.credential_matches_for_test("REVEL_SESSION=unsafe-b"));
        assert!(
            fs::read_to_string(external.path())
                .unwrap()
                .starts_with("REVEL_SESSION=external-safe")
        );
    }

    #[test]
    fn auth_store_rejects_nonregular_cookie_and_lock_entries() {
        for entry in ["cookie", AUTH_LOCK_FILE] {
            let temp = tempfile::tempdir().unwrap();
            let (location, session) = managed_session(temp.path(), "REVEL_SESSION=type-a");
            if entry == "cookie" {
                fs::remove_file(&location.file).unwrap();
            }
            fs::create_dir(location.state_dir.join(entry)).unwrap();

            observe(
                &session,
                &["REVEL_SESSION=type-b; Path=/"],
                "https://atcoder.jp/",
            );
            assert_eq!(session.persist_pending(), [AuthPersistenceWarning::Unsafe]);
            assert!(session.credential_matches_for_test("REVEL_SESSION=type-b"));
            assert!(location.state_dir.join(entry).is_dir());
        }
    }

    #[test]
    fn explicit_write_creates_replaces_and_repairs_regular_entries() {
        let temp = tempfile::tempdir().unwrap();
        let location = location(temp.path());
        let first = parse_pasted_credential("created-a").unwrap();
        let second = parse_pasted_credential("REVEL_SESSION=replaced-b\n").unwrap();

        let snapshot = write_explicit_credential(&location, &first).unwrap();
        assert!(matches!(snapshot.as_ref(), AuthSnapshot::Configured { .. }));
        assert!(location.file.is_file());
        assert!(
            fs::read_to_string(&location.file)
                .unwrap()
                .starts_with("REVEL_SESSION=created-a")
        );

        write_explicit_credential(&location, &second).unwrap();
        assert!(
            fs::read_to_string(&location.file)
                .unwrap()
                .starts_with("REVEL_SESSION=replaced-b")
        );

        for malformed in [b"malformed".as_slice(), b"", &[0xff]] {
            write_cookie_file(&location.file, malformed);
            write_explicit_credential(&location, &first).unwrap();
            assert!(matches!(
                load_auth_snapshot_from(&location),
                AuthSnapshot::Configured { .. }
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn explicit_write_repairs_permissions_even_when_value_is_unchanged() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let location = location(temp.path());
        fs::create_dir_all(&location.state_dir).unwrap();
        fs::write(&location.file, "REVEL_SESSION=same-value").unwrap();
        fs::set_permissions(&location.file, fs::Permissions::from_mode(0o644)).unwrap();
        let credential = parse_pasted_credential("same-value").unwrap();

        write_explicit_credential(&location, &credential).unwrap();

        assert_eq!(
            fs::metadata(&location.file).unwrap().permissions().mode() & 0o077,
            0
        );
    }

    #[test]
    fn explicit_reset_removes_any_regular_contents_and_keeps_missing_hierarchy_missing() {
        for contents in [
            b"REVEL_SESSION=valid".as_slice(),
            b"malformed",
            b"",
            &[0xff],
        ] {
            let temp = tempfile::tempdir().unwrap();
            let location = location(temp.path());
            fs::create_dir_all(&location.state_dir).unwrap();
            write_cookie_file(&location.file, contents);
            assert_eq!(
                reset_explicit_credential(&location).unwrap(),
                ExplicitResetOutcome::Removed
            );
            assert!(!location.file.exists());
        }

        let temp = tempfile::tempdir().unwrap();
        let location = location(temp.path());
        assert_eq!(
            reset_explicit_credential(&location).unwrap(),
            ExplicitResetOutcome::AlreadyMissing
        );
        assert!(!location.platform_base.exists());
    }

    #[test]
    fn explicit_mutations_reject_unsafe_cookie_and_lock_entries() {
        let credential = parse_pasted_credential("safe-new").unwrap();
        for entry in ["cookie", AUTH_LOCK_FILE] {
            let temp = tempfile::tempdir().unwrap();
            let location = location(temp.path());
            fs::create_dir_all(&location.state_dir).unwrap();
            fs::create_dir(location.state_dir.join(entry)).unwrap();

            assert!(write_explicit_credential(&location, &credential).is_err());
            assert!(reset_explicit_credential(&location).is_err());
            assert!(location.state_dir.join(entry).is_dir());
        }
    }

    #[test]
    fn explicit_mutations_reject_symlinks_and_unsafe_parent_hierarchy() {
        let credential = parse_pasted_credential("explicit-safe").unwrap();

        let temp = tempfile::tempdir().unwrap();
        let external = tempfile::NamedTempFile::new().unwrap();
        write_cookie_file(external.path(), "REVEL_SESSION=external-safe");
        let symlink_location = location(temp.path());
        fs::create_dir_all(&symlink_location.state_dir).unwrap();
        if create_file_symlink(external.path(), &symlink_location.file) {
            assert!(write_explicit_credential(&symlink_location, &credential).is_err());
            assert!(reset_explicit_credential(&symlink_location).is_err());
            assert!(
                fs::read_to_string(external.path())
                    .unwrap()
                    .starts_with("REVEL_SESSION=external-safe")
            );
        }

        let temp = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let parent_location = location(temp.path());
        fs::create_dir_all(parent_location.state_dir.parent().unwrap()).unwrap();
        if create_directory_symlink(external.path(), &parent_location.state_dir) {
            assert!(write_explicit_credential(&parent_location, &credential).is_err());
            assert!(reset_explicit_credential(&parent_location).is_err());
            assert!(!external.path().join("cookie").exists());
        }
    }

    #[test]
    fn concurrent_explicit_writers_publish_complete_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let location = location(temp.path());
        let initial = parse_pasted_credential("explicit-initial").unwrap();
        write_explicit_credential(&location, &initial).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut writers = Vec::new();
        for value in ["explicit-one", "explicit-two"] {
            let location = location.clone();
            let barrier = Arc::clone(&barrier);
            writers.push(std::thread::spawn(move || {
                let credential = parse_pasted_credential(value).unwrap();
                barrier.wait();
                write_explicit_credential(&location, &credential).unwrap();
            }));
        }
        barrier.wait();
        for writer in writers {
            writer.join().unwrap();
        }

        let final_value = fs::read_to_string(&location.file).unwrap();
        assert!(matches!(
            final_value.as_str(),
            "REVEL_SESSION=explicit-one" | "REVEL_SESSION=explicit-two"
        ));
        assert_no_auth_staging_files(&location.state_dir);
    }

    #[test]
    fn explicit_write_returns_its_own_locked_read_back_before_a_competing_writer() {
        let temp = tempfile::tempdir().unwrap();
        let location = location(temp.path());
        let first_location = location.clone();
        let (published_tx, published_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first = std::thread::spawn(move || {
            let store = AuthStore::open_or_create(&first_location).unwrap();
            let credential = parse_pasted_credential("atomic-first").unwrap();
            let read_back = store
                .write_explicit_with(&credential, |directory, credential| {
                    replace_cookie_file(directory, credential)?;
                    published_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                })
                .unwrap();
            read_back.same_value(&credential)
        });

        published_rx.recv().unwrap();
        let second_location = location.clone();
        let (second_done_tx, second_done_rx) = std::sync::mpsc::channel();
        let second = std::thread::spawn(move || {
            let credential = parse_pasted_credential("atomic-second").unwrap();
            let result = write_explicit_credential(&second_location, &credential);
            second_done_tx.send(result.is_ok()).unwrap();
        });

        let second_was_blocked = second_done_rx
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err();
        release_tx.send(()).unwrap();
        assert!(first.join().unwrap());
        assert!(second_was_blocked);
        assert!(second_done_rx.recv().unwrap());
        second.join().unwrap();
    }

    #[test]
    fn explicit_write_faults_before_publish_preserve_the_old_credential() {
        for injected in [
            AuthStoreWriteStage::StagingCreate,
            AuthStoreWriteStage::Write,
            AuthStoreWriteStage::FileSync,
            AuthStoreWriteStage::Publish,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let (location, _) = managed_session(temp.path(), "REVEL_SESSION=explicit-old");
            let store = AuthStore::open(&location).unwrap().unwrap();
            let replacement = parse_pasted_credential("explicit-new").unwrap();

            let result = store.write_explicit_with(&replacement, |directory, credential| {
                replace_cookie_file_with_hook(directory, credential, |stage, _, _| {
                    if stage == injected {
                        Err(io::Error::other("injected explicit write failure"))
                    } else {
                        Ok(())
                    }
                })
            });

            assert!(result.is_err());
            assert!(
                fs::read_to_string(&location.file)
                    .unwrap()
                    .starts_with("REVEL_SESSION=explicit-old")
            );
            assert_no_auth_staging_files(&location.state_dir);
        }
    }

    #[test]
    fn explicit_write_and_server_cas_share_the_same_lock_and_preserve_explicit_intent() {
        let temp = tempfile::tempdir().unwrap();
        let (location, session) = managed_session(temp.path(), "REVEL_SESSION=race-a");
        let explicit_location = location.clone();
        let writer = std::thread::spawn(move || {
            let credential = parse_pasted_credential("explicit-c").unwrap();
            write_explicit_credential(&explicit_location, &credential).unwrap();
        });

        observe(
            &session,
            &["REVEL_SESSION=server-b; Path=/"],
            "https://atcoder.jp/settings",
        );
        let _ = session.persist_pending();
        writer.join().unwrap();

        assert!(
            fs::read_to_string(&location.file)
                .unwrap()
                .starts_with("REVEL_SESSION=explicit-c")
        );
    }

    #[test]
    fn reset_prevents_an_old_server_cas_from_recreating_the_cookie() {
        let temp = tempfile::tempdir().unwrap();
        let (location, session) = managed_session(temp.path(), "REVEL_SESSION=reset-a");
        observe(
            &session,
            &["REVEL_SESSION=server-b; Path=/"],
            "https://atcoder.jp/settings",
        );
        assert_eq!(
            reset_explicit_credential(&location).unwrap(),
            ExplicitResetOutcome::Removed
        );

        assert_eq!(session.persist_pending(), [AuthPersistenceWarning::Missing]);
        assert!(!location.file.exists());
    }

    #[test]
    fn atomic_publish_failure_preserves_competitor_and_cleans_owned_staging() {
        let temp = tempfile::tempdir().unwrap();
        let (location, _) = managed_session(temp.path(), "REVEL_SESSION=publish-a");
        let store = AuthStore::open(&location).unwrap().unwrap();
        let replacement = Credential::from_cookie_value("publish-b").unwrap();

        let error = replace_cookie_file_with_hook(
            &store.directory,
            &replacement,
            |stage, directory, staged_name| {
                if stage != AuthStoreWriteStage::Publish {
                    return Ok(());
                }
                let _staged_name = staged_name.expect("publish stage must have a staging name");
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let metadata = fs::metadata(location.state_dir.join(_staged_name))?;
                    assert_eq!(metadata.permissions().mode() & 0o077, 0);
                }
                directory.remove_file("cookie")?;
                directory.create_dir("cookie")?;
                Ok(())
            },
        )
        .unwrap_err();

        assert!(!error.to_string().contains("publish-a"));
        assert!(!error.to_string().contains("publish-b"));
        assert!(location.file.is_dir());
        assert!(
            fs::read_dir(&location.state_dir)
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(AUTH_STAGING_PREFIX))
        );
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_cookie_permissions_block_cas_without_replacement() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let (location, session) = managed_session(temp.path(), "REVEL_SESSION=mode-a");
        fs::set_permissions(&location.file, fs::Permissions::from_mode(0o644)).unwrap();
        observe(
            &session,
            &["REVEL_SESSION=mode-b; Path=/"],
            "https://atcoder.jp/",
        );

        assert_eq!(session.persist_pending(), [AuthPersistenceWarning::Unsafe]);
        assert_eq!(
            fs::metadata(location.file).unwrap().permissions().mode() & 0o077,
            0o044
        );
    }

    #[cfg(unix)]
    #[test]
    fn auth_store_replacement_is_private_and_pinned_before_path_swap() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let (location, session) = managed_session(temp.path(), "REVEL_SESSION=pinned-a");
        write_cookie_file(&external.path().join("cookie"), "REVEL_SESSION=external-x");
        let moved_state = location.state_dir.with_file_name("moved-state");
        fs::rename(&location.state_dir, &moved_state).unwrap();
        assert!(create_directory_symlink(
            external.path(),
            &location.state_dir
        ));

        observe(
            &session,
            &["REVEL_SESSION=pinned-b; Path=/"],
            "https://atcoder.jp/",
        );
        assert!(session.persist_pending().is_empty());

        let metadata = fs::metadata(moved_state.join("cookie")).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o077, 0);
        assert!(
            fs::read_to_string(moved_state.join("cookie"))
                .unwrap()
                .starts_with("REVEL_SESSION=pinned-b")
        );
        assert!(
            fs::read_to_string(external.path().join("cookie"))
                .unwrap()
                .starts_with("REVEL_SESSION=external-x")
        );
    }
}
