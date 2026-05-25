use std::{io, path::Path};

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    net::UnixStream,
};

use crate::{codec, Request, Response};

pub async fn request(socket_path: &Path, names: &[String]) -> io::Result<Vec<Response>> {
    let stream = UnixStream::connect(socket_path).await?;
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    for (id, name) in names.iter().enumerate() {
        let req = Request {
            id: id as u64,
            name: name.clone(),
        };
        let bytes = codec::encode(&req)?;
        writer.write_all(&bytes).await?;
    }
    writer.flush().await?;
    drop(writer);

    let mut responses = Vec::with_capacity(names.len());
    let mut line = String::new();

    for _ in 0..names.len() {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before all responses received",
            ));
        }

        let response = serde_json::from_str(line.trim_end())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        responses.push(response);
    }

    Ok(responses)
}
