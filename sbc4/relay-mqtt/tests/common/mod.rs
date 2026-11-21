//! Helpers shared by the integration tests.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use relay_mqtt::bridge::Mqtt;
use relay_mqtt::fake_controller::{bind, run, FakeState, Shared};
use tokio::task::JoinHandle;

pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        // Keep it short: sun_path is ~104 bytes on macOS.
        let p = std::env::temp_dir().join(format!(
            "rmq-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    pub fn sock(&self) -> PathBuf {
        self.0.join("ctl.sock")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Default)]
pub struct RecMqtt(pub Arc<Mutex<Vec<(String, String)>>>);

impl RecMqtt {
    pub fn take(&self) -> Vec<(String, String)> {
        std::mem::take(&mut self.0.lock().unwrap())
    }
}

impl Mqtt for RecMqtt {
    async fn publish_retained(&mut self, topic: &str, payload: &'static str) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .push((topic.to_string(), payload.to_string()));
        Ok(())
    }
    async fn subscribe(&mut self, _filter: &str) -> Result<()> {
        Ok(())
    }
}

pub fn start_fake(sock: &std::path::Path, state: FakeState) -> (Shared, JoinHandle<()>) {
    let shared: Shared = Arc::new(Mutex::new(state));
    let l = bind(sock).unwrap();
    let s2 = shared.clone();
    let h = tokio::spawn(async move {
        let _ = run(l, s2).await;
    });
    (shared, h)
}
