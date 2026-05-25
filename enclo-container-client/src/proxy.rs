use std::os::unix::process::CommandExt;
use crate::util;

pub fn run(env: bool, cmd: &[String]) -> anyhow::Result<()> {
    let proxy_url = std::env::var("INSECTJAR_PROXY")
        .map_err(|_| anyhow::anyhow!("INSECTJAR_PROXY is not set"))?;

    if proxy_url.is_empty() {
        anyhow::bail!("INSECTJAR_PROXY is empty");
    }

    if env {
        let vars = [("HTTP_PROXY", &proxy_url), ("HTTPS_PROXY", &proxy_url),
                    ("http_proxy", &proxy_url), ("https_proxy", &proxy_url)];
        for (k, v) in vars {
            println!("export {}={}", k, util::shell_quote(v));
        }
        return Ok(());
    }

    let err = std::process::Command::new(&cmd[0])
        .args(&cmd[1..])
        .env("HTTP_PROXY", &proxy_url)
        .env("HTTPS_PROXY", &proxy_url)
        .env("http_proxy", &proxy_url)
        .env("https_proxy", &proxy_url)
        .exec();

    Err(anyhow::anyhow!("failed to exec '{}': {err}", cmd[0]))
}
