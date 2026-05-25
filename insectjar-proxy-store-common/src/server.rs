use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use crate::{codec, Request, Response, ResponseResult};

/// Run the stdin/stdout server loop. Calls `handler` for each incoming request
/// and writes the returned Response back to stdout.
pub async fn run<F, Fut>(mut handler: F) -> io::Result<()>
where
    F: FnMut(u64, String) -> Fut,
    Fut: std::future::Future<Output = ResponseResult>,
{
    let mut stdin = tokio::io::stdin();
    let mut writer = BufWriter::new(tokio::io::stdout());
    let mut buf: Vec<u8> = Vec::with_capacity(4096);

    loop {
        let mut tmp = [0u8; 4096];
        let n = stdin.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);

        loop {
            match codec::decode_one::<Request>(&buf)? {
                None => break,
                Some((req, consumed)) => {
                    buf.drain(..consumed);
                    let id = req.id;
                    let result = handler(req.id, req.name).await;
                    let resp = Response { id, result };
                    let bytes = codec::encode(&resp)?;
                    writer.write_all(&bytes).await?;
                    writer.flush().await?;
                }
            }
        }
    }

    Ok(())
}
