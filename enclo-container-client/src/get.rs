use std::{collections::HashMap, path::Path, path::PathBuf};
use enclo_backend_protocol::{unix_client, ResponseResult};
use crate::util;

fn silent_error() -> ! {
    std::process::exit(1);
}

/// Fetch secrets from the daemon and return them as a name→value map.
pub async fn fetch(socket_path: &Path, names: &[String]) -> anyhow::Result<HashMap<String, String>> {
    let responses = unix_client::request(socket_path, names)
        .await
        .map_err(|e| anyhow::anyhow!("failed to query {}: {e}", socket_path.display()))?;
    let mut results: HashMap<String, String> = HashMap::new();

    for resp in responses {
        let name = names.get(resp.id as usize)
            .ok_or_else(|| anyhow::anyhow!("response id {} out of range", resp.id))?;

        match resp.result {
            ResponseResult::Ok { value } => { results.insert(name.clone(), value); }
            ResponseResult::NotFound => anyhow::bail!("secret '{}' not found", name),
            ResponseResult::Error { .. } => silent_error(),
        }
    }

    Ok(results)
}

pub async fn run(socket_path: PathBuf, names: Vec<String>, env: bool) -> anyhow::Result<()> {
    if names.is_empty() {
        if !env { println!("{{}}"); }
        return Ok(());
    }
    let results = fetch(&socket_path, &names).await?;
    if env {
        for (k, v) in &results {
            println!("export {}={}", k, util::shell_quote(v));
        }
    } else {
        println!("{}", serde_json::to_string(&results)?);
    }
    Ok(())
}
