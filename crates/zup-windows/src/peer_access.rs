use std::io;
use std::marker::PhantomData;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::rc::Rc;
use std::sync::Arc;

use futures_util::TryStreamExt;
use windows::Win32::Foundation::{ERROR_NO_TOKEN, HANDLE};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, TOKEN_DUPLICATE, TOKEN_IMPERSONATE, TOKEN_QUERY,
    TokenImpersonation,
};
use windows::Win32::System::Threading::{
    GetCurrentThread, OpenProcess, OpenProcessToken, OpenThreadToken,
    PROCESS_QUERY_LIMITED_INFORMATION, SetThreadToken,
};

#[derive(Clone, Debug)]
pub(crate) struct PeerAccess(Arc<OwnedHandle>);

impl PeerAccess {
    pub fn for_process(pid: u32, expected_sid: &str) -> io::Result<Self> {
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }?;
        let process = unsafe { OwnedHandle::from_raw_handle(process.0) };
        let mut token = HANDLE::default();
        unsafe {
            OpenProcessToken(
                HANDLE(process.as_raw_handle()),
                TOKEN_QUERY | TOKEN_DUPLICATE,
                &mut token,
            )?;
        }
        let token = unsafe { OwnedHandle::from_raw_handle(token.0) };
        let sid = crate::transport::UserSid::from_token(&token).map_err(io::Error::other)?;
        if sid.display() != expected_sid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "parent token SID mismatch",
            ));
        }
        let mut duplicate = HANDLE::default();
        unsafe {
            DuplicateTokenEx(
                HANDLE(token.as_raw_handle()),
                TOKEN_QUERY | TOKEN_IMPERSONATE,
                None,
                SecurityImpersonation,
                TokenImpersonation,
                &mut duplicate,
            )?;
        }
        Ok(Self(Arc::new(unsafe {
            OwnedHandle::from_raw_handle(duplicate.0)
        })))
    }

    pub fn enter(&self) -> io::Result<PeerGuard> {
        let mut previous = HANDLE::default();
        let previous = match unsafe {
            OpenThreadToken(
                GetCurrentThread(),
                TOKEN_QUERY | TOKEN_IMPERSONATE,
                true,
                &mut previous,
            )
        } {
            Ok(()) => Some(unsafe { OwnedHandle::from_raw_handle(previous.0) }),
            Err(error) if error.code().0 as u32 & 0xffff == ERROR_NO_TOKEN.0 => None,
            Err(error) => return Err(error.into()),
        };
        unsafe {
            SetThreadToken(None, Some(HANDLE(self.0.as_raw_handle())))?;
        }
        Ok(PeerGuard {
            previous,
            _thread_bound: PhantomData,
        })
    }
}

pub(crate) struct PeerGuard {
    previous: Option<OwnedHandle>,
    _thread_bound: PhantomData<Rc<()>>,
}

impl Drop for PeerGuard {
    fn drop(&mut self) {
        let previous = self
            .previous
            .as_ref()
            .map(|token| HANDLE(token.as_raw_handle()));
        if unsafe { SetThreadToken(None, previous) }.is_err() {
            // Continuing under unintended authority is unsafe, including during unwinding.
            std::process::abort();
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PeerTransport {
    pub peer: PeerAccess,
    pub network: tough::DefaultTransport,
}

#[async_trait::async_trait]
impl tough::Transport for PeerTransport {
    async fn fetch(&self, url: url::Url) -> Result<tough::TransportStream, tough::TransportError> {
        use tough::{SafeUrlPath, TransportError, TransportErrorKind};
        if url.scheme() != "file" {
            return self.network.fetch(url).await;
        }
        let path = url.safe_url_filepath();
        if !path.is_absolute() {
            return Err(TransportError::new(TransportErrorKind::Other, &url));
        }
        let peer = self.peer.clone();
        let file = tokio::task::spawn_blocking(move || {
            let _guard = peer.enter()?;
            std::fs::File::open(path)
        })
        .await
        .map_err(|error| TransportError::new_with_cause(TransportErrorKind::Other, &url, error))?
        .map_err(|error| {
            let kind = if error.kind() == io::ErrorKind::NotFound {
                TransportErrorKind::FileNotFound
            } else {
                TransportErrorKind::Other
            };
            TransportError::new_with_cause(kind, &url, error)
        })?;
        let stream = tokio_util::io::ReaderStream::new(tokio::fs::File::from_std(file)).map_err(
            move |error| TransportError::new_with_cause(TransportErrorKind::Other, &url, error),
        );
        Ok(Box::pin(stream))
    }
}

pub(crate) struct PeerPayloadSource {
    pub source: crate::AutoPayloadSource,
    pub peer: PeerAccess,
}

impl zup_bundle::PayloadSource for PeerPayloadSource {
    fn open(
        &self,
        path: &zup_core::RelativePath,
        digest: &zup_core::Sha256Digest,
        size: u64,
    ) -> Result<zup_bundle::PayloadReader, zup_bundle::PayloadError> {
        let _guard = self
            .peer
            .enter()
            .map_err(|source| zup_bundle::PayloadError::Read {
                path: path.to_string(),
                source,
            })?;
        self.source.open(path, digest, size)
    }
}

pub(crate) struct PeerOperationHost<'a> {
    pub inner: crate::operation_host::WindowsOperationHost<'a>,
    pub peer: &'a PeerAccess,
}

impl zup_runtime::OperationHost for PeerOperationHost<'_> {
    fn collected_deployments(
        &self,
        input: &zup_runtime::OperationInput,
        record: &zup_plan::InstallationRecord,
    ) -> Result<Vec<zup_plan::RetiredDeploymentSelection>, String> {
        self.inner.collected_deployments(input, record)
    }

    fn cleanup_candidates(
        &self,
        input: &zup_runtime::OperationInput,
        record: &zup_plan::InstallationRecord,
        retiring: Option<&zup_plan::RetentionChange>,
    ) -> Result<Vec<zup_plan::CleanupOperation>, String> {
        self.inner.cleanup_candidates(input, record, retiring)
    }

    fn data_bindings(
        &self,
        target: &zup_plan::BoundDeployment,
    ) -> Result<std::collections::BTreeMap<zup_core::ResourceId, zup_plan::PersistentBinding>, String>
    {
        self.inner.data_bindings(target)
    }
    fn persistent_claims(
        &self,
        input: &zup_runtime::OperationInput,
    ) -> Result<Option<zup_plan::PersistentClaimSelection>, zup_runtime::OperationError> {
        self.inner.persistent_claims(input)
    }
    fn plan_build(
        &self,
        build: &zup_core::TargetBuildPlan,
        request: &zup_plan::PlanRequest,
        cancellation: &dyn zup_plan::CancellationQuery,
    ) -> Result<zup_plan::PlannedInstallation, zup_runtime::OperationError> {
        let _guard = self
            .peer
            .enter()
            .map_err(|error| zup_runtime::OperationError::refused("source access", error))?;
        self.inner.plan_build(build, request, cancellation)
    }

    fn attach_content(
        &self,
        target: &mut zup_plan::BoundDeployment,
        content: &zup_runtime::ContentAccess,
        preset: Option<&zup_core::PresetRuntime>,
    ) -> Result<(), String> {
        let _guard = self.peer.enter().map_err(|error| error.to_string())?;
        self.inner.attach_content(target, content, preset)
    }

    fn resolve_target(
        &self,
        install: &zup_plan::DesiredState,
    ) -> Result<zup_plan::BoundDeployment, String> {
        self.inner.resolve_target(install)
    }

    fn observe(
        &self,
        target: Option<&zup_plan::BoundDeployment>,
        ledger: Option<&zup_plan::InstallationRecord>,
    ) -> Result<
        (
            Option<zup_plan::HostSnapshot>,
            std::collections::BTreeMap<zup_core::ResourceKey, bool>,
        ),
        String,
    > {
        self.inner.observe(target, ledger)
    }

    fn compile(
        &self,
        execution: &zup_plan::ExecutionPlan,
        target: Option<&zup_plan::BoundDeployment>,
        ledger: Option<&zup_plan::InstallationRecord>,
    ) -> Result<zup_plan::DeploymentEffects, String> {
        self.inner.compile(execution, target, ledger)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use windows::Win32::Security::{
        CreateRestrictedToken, CreateWellKnownSid, DISABLE_MAX_PRIVILEGE, PSID, SID_AND_ATTRIBUTES,
        WinNullSid,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;
    use zup_bundle::PayloadSource;

    fn restricted() -> PeerAccess {
        let mut token = HANDLE::default();
        unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_QUERY | TOKEN_DUPLICATE,
                &mut token,
            )
            .unwrap();
        }
        let token = unsafe { OwnedHandle::from_raw_handle(token.0) };
        let mut sid = [0u32; 17];
        let mut size = std::mem::size_of_val(&sid) as u32;
        unsafe {
            CreateWellKnownSid(
                WinNullSid,
                None,
                Some(PSID(sid.as_mut_ptr().cast())),
                &mut size,
            )
            .unwrap();
        }
        let mut limited = HANDLE::default();
        unsafe {
            CreateRestrictedToken(
                HANDLE(token.as_raw_handle()),
                DISABLE_MAX_PRIVILEGE,
                None,
                None,
                Some(&[SID_AND_ATTRIBUTES {
                    Sid: PSID(sid.as_mut_ptr().cast()),
                    Attributes: 0,
                }]),
                &mut limited,
            )
            .unwrap();
        }
        let limited = unsafe { OwnedHandle::from_raw_handle(limited.0) };
        let mut duplicate = HANDLE::default();
        unsafe {
            DuplicateTokenEx(
                HANDLE(limited.as_raw_handle()),
                TOKEN_QUERY | TOKEN_IMPERSONATE,
                None,
                SecurityImpersonation,
                TokenImpersonation,
                &mut duplicate,
            )
            .unwrap();
        }
        PeerAccess(Arc::new(unsafe {
            OwnedHandle::from_raw_handle(duplicate.0)
        }))
    }

    #[tokio::test]
    async fn source_access_uses_peer_authority_and_restores_the_thread_after_failure() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("content");
        std::fs::write(&path, b"owned input").unwrap();
        let cache = zup_acquire::ContentCache::open(
            root.path().join("cache"),
            zup_acquire::CachePolicy::Auto,
        )
        .unwrap();
        let digest = zup_core::hash_bytes(b"expected");
        let cached = cache
            .root()
            .join(zup_acquire::blob_path(&digest).to_string());
        std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::fs::write(&cached, b"invalid").unwrap();
        let peer = restricted();
        let source = PeerPayloadSource {
            source: crate::AutoPayloadSource::from_path(root.path()).unwrap(),
            peer: peer.clone(),
        };
        assert!(
            source
                .open(
                    &zup_core::RelativePath::new("content").unwrap(),
                    &zup_core::hash_bytes(b"owned input"),
                    11
                )
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"owned input");
        let cache_root = cache.root().to_path_buf();
        drop(cache);
        let result = std::panic::catch_unwind(|| {
            let cache =
                zup_acquire::ContentCache::open(&cache_root, zup_acquire::CachePolicy::Auto)
                    .unwrap();
            let _guard = peer.enter().unwrap();
            let _nested = peer.enter().unwrap();
            let descriptor = zup_acquire::ContentDescriptor::stored(
                zup_acquire::ContentKind::Payload,
                digest,
                8,
            );
            let _ = cache.get(&descriptor, zup_acquire::Verify::Full);
            assert!(
                zup_acquire::ContentCache::open(
                    root.path().join("new-cache"),
                    zup_acquire::CachePolicy::Auto
                )
                .is_err()
            );
            panic!("interrupted source verification");
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read(&cached).unwrap(), b"invalid");
        assert!(!root.path().join("new-cache").exists());
        let mut file = std::fs::File::open(&path).unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"owned input");
        let transport = PeerTransport {
            peer,
            network: tough::DefaultTransport::default(),
        };
        let error = tough::Transport::fetch(&transport, url::Url::from_file_path(&path).unwrap())
            .await
            .err()
            .expect("restricted local read refused");
        assert_eq!(error.kind(), tough::TransportErrorKind::Other);
        assert_eq!(std::fs::read(&path).unwrap(), b"owned input");
        let sid = crate::transport::UserSid::current().unwrap();
        let transport = PeerTransport {
            peer: PeerAccess::for_process(std::process::id(), sid.display()).unwrap(),
            network: tough::DefaultTransport::default(),
        };
        let stream = tough::Transport::fetch(&transport, url::Url::from_file_path(&path).unwrap())
            .await
            .unwrap();
        let chunks: Vec<_> = stream.try_collect().await.unwrap();
        assert_eq!(chunks.concat(), b"owned input");
        let repository = root.path().join("repository");
        std::fs::create_dir(&repository).unwrap();
        let base = url::Url::from_directory_path(&repository).unwrap();
        let traversal = base.join("..%2Fcontent").unwrap();
        assert!(
            tough::Transport::fetch(&transport, traversal)
                .await
                .is_err()
        );
    }

    #[test]
    fn parent_token_must_match_the_authenticated_sid() {
        assert!(PeerAccess::for_process(std::process::id(), "S-1-0-0").is_err());
        let sid = crate::transport::UserSid::current().unwrap();
        let peer = PeerAccess::for_process(std::process::id(), sid.display()).unwrap();
        let _guard = peer.enter().unwrap();
    }
}
