use std::path::Path;

pub async fn run(socket: &Path, profile: &str) -> anyhow::Result<()> {
    let name = format!("AWS_CREDENTIALS?profile={}", urlencoding::encode(profile));
    let mut secrets = crate::get::fetch(socket, std::slice::from_ref(&name)).await?;
    let credentials = secrets
        .remove(&name)
        .ok_or_else(|| anyhow::anyhow!("credentials provider returned no response"))?;
    // The provider returns the credential_process document, not a secret map.
    println!("{credentials}");
    Ok(())
}
