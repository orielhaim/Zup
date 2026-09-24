use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::RuntimeRequest;
use serde_json::json;
use tracing_appender::non_blocking::{NonBlocking, NonBlockingBuilder, WorkerGuard};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use uuid::Uuid;
use zup_core::SelectedScope;

pub struct SessionLog {
    path: PathBuf,
    operation: String,
    session_id: Uuid,
    writer: Arc<Mutex<NonBlocking>>,
    _guard: WorkerGuard,
}

impl SessionLog {
    pub fn start(request: &RuntimeRequest, operation: &str) -> Option<Self> {
        let session_id = Uuid::now_v7();
        let directory = std::env::temp_dir().join("zup").join("sessions");
        std::fs::create_dir_all(&directory).ok()?;
        let appender =
            RollingFileAppender::new(Rotation::NEVER, &directory, format!("{session_id}.log"));
        let (writer, guard) = NonBlockingBuilder::default().lossy(true).finish(appender);
        let log = Self {
            path: directory.join(format!("{session_id}.log")),
            operation: operation.to_owned(),
            session_id,
            writer: Arc::new(Mutex::new(writer)),
            _guard: guard,
        };
        log.event(
            "started",
            json!({
                "operation": operation,
                "app_id": request.app_id.as_str(),
                "app_version": request.app_version.to_string(),
                "scope": request.scope,
                "transaction_id": request.recovery_id.map(|id| id.to_string()),
                "state_root": request.state_root.display().to_string(),
            }),
        );
        Some(log)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn event(&self, name: &str, detail: serde_json::Value) {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_millis())
            .unwrap_or_default();
        let line = json!({
            "timestamp_ms": timestamp,
            "session_id": self.session_id,
            "event": name,
            "detail": detail,
        });
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writeln!(writer, "{line}");
            let _ = writer.flush();
        }
        tracing::info!(session_id = %self.session_id, event = name, "session diagnostic");
    }

    pub fn summary(&self, request: &RuntimeRequest) -> String {
        format!(
            "zup session {}\noperation: {}\napp: {} {}\nscope: {}\nlog: {}",
            self.session_id,
            self.operation,
            request.app_id,
            request.app_version,
            scope_label(request.scope),
            self.path.display()
        )
    }
}

fn scope_label(scope: SelectedScope) -> &'static str {
    match scope {
        SelectedScope::User => "user",
        SelectedScope::Machine => "machine",
    }
}
