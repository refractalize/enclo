use std::{collections::HashMap, path::PathBuf};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    net::UnixStream,
};
use insectjar_proxy_store_common::{codec, Request, Response, ResponseResult};

pub async fn run(socket_path: PathBuf, names: Vec<String>) -> anyhow::Result<()> {
    if names.is_empty() {
        println!("{{}}");
        return Ok(());
    }

    let stream = UnixStream::connect(&socket_path).await
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
    // Signal we're done sending so the server can close the write side cleanly.
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
            ResponseResult::Ok { value } => {
                results.insert(name.clone(), value);
            }
            ResponseResult::Err { error } => {
                anyhow::bail!("secret '{}': {}", name, error);
            }
        }
    }

    println!("{}", serde_json::to_string(&results)?);
    Ok(())
}
