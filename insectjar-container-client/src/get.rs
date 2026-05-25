use std::{collections::HashMap, path::Path, path::PathBuf};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    net::UnixStream,
};
use insectjar_proxy_store_common::{codec, Request, Response, ResponseResult};

fn silent_error() -> ! {
    std::process::exit(1);
}

/// Fetch secrets from the daemon and return them as a name→value map.
pub async fn fetch(socket_path: &Path, names: &[String]) -> anyhow::Result<HashMap<String, String>> {
    let stream = UnixStream::connect(socket_path).await
        .map_err(|e| anyhow::anyhow!("failed to connect to {}: {e}", socket_path.display()))?;

    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    for (id, name) in names.iter().enumerate() {
        let req = Request { id: id as u64, name: name.clone() };
        let bytes = codec::encode(&req)?;
        writer.write_all(&bytes).await?;
    }
    writer.flush().await?;
    drop(writer);

    let mut results: HashMap<String, String> = HashMap::new();
    let mut line = String::new();

    for _ in 0..names.len() {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            anyhow::bail!("connection closed before all responses received");
        }

        let resp: Response = serde_json::from_str(line.trim_end())
            .map_err(|e| anyhow::anyhow!("invalid response from server: {e}"))?;

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
            println!("export {}={}", k, shell_quote(v));
        }
    } else {
        println!("{}", serde_json::to_string(&results)?);
    }
    Ok(())
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}
