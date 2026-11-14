use std::io::Seek;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::FileExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use zup_core::TargetPath;
use zup_plan::NativeCacheOperation;
use zup_transaction::{NativeCacheCompletion, OperationReceipt};

use crate::error::ExecError;
use crate::fs::{OwnedDirectory, StreamedRegular};

pub struct PreparedCache {
    intent: zup_core::Sha256Digest,
    directory: OwnedDirectory,
    lease: std::fs::File,
    tool: StreamedRegular,
    tool_path: TargetPath,
}

impl PreparedCache {
    pub fn prepare(operation: &NativeCacheOperation) -> Result<Self, ExecError> {
        if rustix::process::geteuid().is_root() {
            return Err(ExecError::RefreshRefused(
                "machine cache regeneration has no authorized native policy".into(),
            ));
        }
        let path = crate::lowering::to_host_path(&operation.directory)?;
        crate::fs::verify_ancestor_trust(&path, rustix::process::geteuid().as_raw())?;
        let directory = OwnedDirectory::open(&path)?;
        let stat = rustix::fs::fstat(directory.as_fd())
            .map_err(|error| ExecError::RefreshRefused(error.to_string()))?;
        if stat.st_uid != rustix::process::geteuid().as_raw() || stat.st_mode & 0o022 != 0 {
            return Err(ExecError::RefreshRefused("cache directory must be owned by the installing user and protected from foreign writes".into()));
        }
        verify_tree(&directory, 0)?;
        let lease = std::fs::File::from(
            rustix::fs::openat(
                directory.as_fd(),
                ".",
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .map_err(|error| ExecError::RefreshRefused(error.to_string()))?,
        );
        lease.try_lock().map_err(|error| {
            ExecError::RefreshRefused(format!(
                "another deployment is changing this native cache: {error}"
            ))
        })?;
        let name = operation.kind.executable_name();
        let tool_directory = Path::new("/usr/bin");
        crate::fs::verify_ancestor_trust(tool_directory, 0)?;
        let tools = OwnedDirectory::open(tool_directory)?;
        let tool = tools
            .stream_regular(name)
            .map_err(|error| ExecError::RefreshUnavailable {
                tool: name.into(),
                reason: error.to_string(),
            })?;
        let mut magic = [0; 4];
        tool.file
            .read_exact_at(&mut magic, 0)
            .map_err(|error| ExecError::RefreshRefused(error.to_string()))?;
        if tool.uid != 0
            || tool.mode & 0o7022 != 0
            || tool.mode & 0o111 == 0
            || magic != *b"\x7fELF"
        {
            return Err(ExecError::RefreshRefused(
                "native cache generator must be a protected root-owned ELF executable".into(),
            ));
        }
        let tool_path = TargetPath::new(*operation.directory.target(), format!("/usr/bin/{name}"))
            .map_err(|error| ExecError::RefreshRefused(error.to_string()))?;
        Ok(Self {
            intent: operation.fingerprint(),
            directory,
            lease,
            tool,
            tool_path,
        })
    }

    fn validate(&self, operation: &NativeCacheOperation) -> Result<(), ExecError> {
        if self.intent != operation.fingerprint() {
            return Err(ExecError::RefreshRefused(
                "prepared cache capability differs from deployment intent".into(),
            ));
        }
        let path = crate::lowering::to_host_path(&operation.directory)?;
        crate::fs::verify_ancestor_trust(&path, rustix::process::geteuid().as_raw())?;
        let current = OwnedDirectory::open(&path)?;
        let held = rustix::fs::fstat(self.directory.as_fd())
            .map_err(|error| ExecError::RefreshRefused(error.to_string()))?;
        let found = rustix::fs::fstat(current.as_fd())
            .map_err(|error| ExecError::RefreshRefused(error.to_string()))?;
        if held.st_dev != found.st_dev
            || held.st_ino != found.st_ino
            || found.st_uid != rustix::process::geteuid().as_raw()
            || found.st_mode & 0o022 != 0
        {
            return Err(ExecError::RefreshRefused(
                "cache directory identity or authority changed".into(),
            ));
        }
        let tool = rustix::fs::fstat(self.tool.file.as_fd())
            .map_err(|error| ExecError::RefreshRefused(error.to_string()))?;
        if tool.st_uid != 0 || tool.st_mode & 0o7022 != 0 || tool.st_mode & 0o111 == 0 {
            return Err(ExecError::RefreshRefused(
                "native cache generator authority changed".into(),
            ));
        }
        let mut file = &self.tool.file;
        file.rewind()
            .map_err(|error| ExecError::RefreshRefused(error.to_string()))?;
        let (size, digest) = zup_core::hash_reader(file)
            .map_err(|error| ExecError::RefreshRefused(error.to_string()))?;
        if size != self.tool.size || digest != self.tool.sha256 {
            return Err(ExecError::RefreshRefused(
                "native cache generator content changed".into(),
            ));
        }
        verify_tree(&self.directory, 0)?;
        Ok(())
    }

    #[allow(unsafe_code)]
    fn command(&self) -> std::io::Result<Command> {
        let mut command = Command::new(format!("/proc/self/fd/{}", self.tool.file.as_raw_fd()));
        command
            .current_dir(format!(
                "/proc/self/fd/{}",
                self.directory.as_fd().as_raw_fd()
            ))
            .arg(".")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let lease = self.lease.try_clone()?;
        // Only the native generator inherits its cache lease, so parent death cannot admit a concurrent writer.
        unsafe {
            command.pre_exec(move || {
                rustix::io::fcntl_setfd(&lease, rustix::io::FdFlags::empty()).map_err(Into::into)
            });
        }
        Ok(command)
    }

    pub fn run(&self, operation: &NativeCacheOperation) -> Result<OperationReceipt, ExecError> {
        self.validate(operation)?;
        let status = self
            .command()
            .and_then(|mut command| command.status())
            .map_err(|error| ExecError::RefreshFailed {
                tool: self.tool_path.to_string(),
                directory: operation.directory.to_string(),
                reason: error.to_string(),
            })?;
        if !status.success() {
            return Err(ExecError::RefreshFailed {
                tool: self.tool_path.to_string(),
                directory: operation.directory.to_string(),
                reason: status.to_string(),
            });
        }
        self.validate(operation)?;
        Ok(OperationReceipt::NativeCache {
            intent: self.intent,
            completion: NativeCacheCompletion::Completed {
                tool: self.tool_path.clone(),
                object: self.tool.object(),
                sha256: self.tool.sha256,
            },
        })
    }

    pub fn verify(
        &self,
        operation: &NativeCacheOperation,
        receipt: &OperationReceipt,
    ) -> Result<(), ExecError> {
        self.validate(operation)?;
        match receipt {
            OperationReceipt::NativeCache {
                intent,
                completion:
                    NativeCacheCompletion::Completed {
                        tool,
                        object,
                        sha256,
                    },
            } if *intent == self.intent
                && tool == &self.tool_path
                && *object == self.tool.object()
                && *sha256 == self.tool.sha256 =>
            {
                Ok(())
            }
            _ => Err(ExecError::RefreshRefused(
                "cache invocation has no matching successful completion evidence".into(),
            )),
        }
    }
}

fn verify_tree(directory: &OwnedDirectory, depth: usize) -> Result<(), ExecError> {
    if depth > 128 {
        return Err(ExecError::RefreshRefused(
            "native cache tree exceeds the supported directory depth".into(),
        ));
    }
    for name in directory.child_names()? {
        let stat = directory.stat_child(&name)?;
        if stat.uid != rustix::process::geteuid().as_raw() || stat.mode & 0o7022 != 0 {
            return Err(ExecError::RefreshRefused(
                "native cache contains an entry with foreign ownership or writable permissions"
                    .into(),
            ));
        }
        match stat.kind {
            crate::fs::EntryKind::Directory => {
                verify_tree(&directory.open_child(&name)?, depth + 1)?
            }
            crate::fs::EntryKind::Regular => {
                directory.stream_regular(&name)?;
            }
            _ => {
                return Err(ExecError::RefreshRefused(
                    "native cache contains a symbolic link or special file".into(),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    struct Generator(std::os::fd::OwnedFd);
    impl Drop for Generator {
        fn drop(&mut self) {
            let _ = rustix::process::pidfd_send_signal(&self.0, rustix::process::Signal::KILL);
        }
    }

    #[test]
    fn native_generator_keeps_its_lease_when_the_installer_dies() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        std::fs::create_dir(root.join("packages")).unwrap();
        std::fs::write(root.join("packages/test.xml"), b"<?xml version=\"1.0\"?><mime-info xmlns=\"http://www.freedesktop.org/standards/shared-mime-info\"/>").unwrap();
        let mut installer = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "refresh::tests::stopped_generator_child",
                "--ignored",
                "--test-threads=1",
            ])
            .env("ZUP_CACHE_PARENT_DEATH_DIRECTORY", root)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let generator = loop {
            let tasks = std::fs::read_dir(format!("/proc/{}/task", installer.id())).unwrap();
            let stopped = tasks.filter_map(Result::ok).find_map(|task| {
                let children = std::fs::read_to_string(task.path().join("children")).ok()?;
                children.split_whitespace().find_map(|child| {
                    let status = std::fs::read_to_string(format!("/proc/{child}/status")).ok()?;
                    if !status.lines().any(|line| line.starts_with("State:\tT")) {
                        return None;
                    }
                    let pid = rustix::process::Pid::from_raw(child.parse().ok()?)?;
                    rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty())
                        .ok()
                        .map(Generator)
                })
            });
            if let Some(generator) = stopped {
                break generator;
            }
            if Instant::now() >= deadline {
                let _ = installer.kill();
                let _ = installer.wait();
                panic!("native generator did not reach the isolated stop boundary");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        installer.kill().unwrap();
        installer.wait().unwrap();
        let directory = std::fs::File::open(root).unwrap();
        assert!(matches!(
            directory.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        rustix::process::pidfd_send_signal(&generator.0, rustix::process::Signal::CONT).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match directory.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                result => panic!("native generator did not release its lease: {result:?}"),
            }
        }
        assert!(root.join("mime.cache").is_file());
    }

    #[test]
    #[ignore = "invoked by the isolated parent-death test"]
    #[allow(unsafe_code)]
    fn stopped_generator_child() {
        let root = std::env::var("ZUP_CACHE_PARENT_DEATH_DIRECTORY").unwrap();
        let operation = NativeCacheOperation {
            kind: zup_plan::NativeCacheKind::Mime,
            directory: TargetPath::new(zup_core::Target::LINUX_X64, &root).unwrap(),
            sources: [zup_core::ResourceKey::File {
                id: zup_core::FileResourceId::Native {
                    id: zup_core::NativeFileId::MimeDefinitions,
                },
            }]
            .into(),
        };
        let prepared = PreparedCache::prepare(&operation).unwrap();
        let mut command = prepared.command().unwrap();
        // The fixture stops after lease inheritance and before exec; only async-signal-safe syscalls run here.
        unsafe {
            command.pre_exec(|| {
                rustix::process::kill_process(
                    rustix::process::getpid(),
                    rustix::process::Signal::STOP,
                )
                .map_err(Into::into)
            });
        }
        assert!(command.status().unwrap().success());
    }
}
