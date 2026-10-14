//! Loopback-only wire qualification runner. Never refreshes or loads real credentials.
use std::{path::Path, sync::Arc};
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("fixture config");
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    anyhow::ensure!(
        v.get("fixtureAccounts").is_some() || v.get("fixtureQuota").is_some(),
        "Fixture quota required"
    );
    let dir = std::env::temp_dir().join(format!("native-fixture-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let cfg = cpa_core::config::Config::parse(&format!("oauth:\n  auth-dir: {}\n", dir.display()))?;
    let rt = Arc::new(cpa_server::testing::runtime(
        cfg,
        Vec::new(),
        cpa_exec::Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("https://127.0.0.1:9")?,
            codex: cpa_exec::codex::CodexExecutor::new()?,
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        },
    ));
    let _running = cpa_server::native_claude::start_config(rt, Path::new(&path)).await?;
    tokio::signal::ctrl_c().await?;
    Ok(())
}
