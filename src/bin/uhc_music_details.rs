//! Read optional music context using this UHC installation's existing pairing.
use std::io::Read;
use unified_hifi_control::cloud_connector::music::{MusicDetailsClient, MusicIdentity};

#[derive(serde::Deserialize)]
struct CurrentItem {
    zone_id: String,
    line1: String,
    line2: String,
    line3: Option<String>,
    length: Option<u32>,
}

async fn now_playing(zone: &str) -> anyhow::Result<MusicIdentity> {
    let mut url = url::Url::parse("http://127.0.0.1:8088/now_playing")?;
    url.query_pairs_mut().append_pair("zone_id", zone);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(3))
        .build()?;
    let mut response = client.get(url).send().await?.error_for_status()?;
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(
            body.len() + chunk.len() <= 64 * 1024,
            "local now-playing response exceeds bound"
        );
        body.extend_from_slice(&chunk);
    }
    let item: CurrentItem = serde_json::from_slice(&body)?;
    anyhow::ensure!(
        item.zone_id == zone && !item.line2.trim().is_empty(),
        "zone has no current artist"
    );
    Ok(MusicIdentity {
        artist: Some(item.line2),
        title: Some(item.line1),
        album: item.line3.filter(|s| !s.trim().is_empty()),
        duration_ms: item.length.map(|n| u64::from(n) * 1000),
        ..Default::default()
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let first = args.next();
    let zone = if first.as_deref() == Some("--zone") {
        Some(
            args.next()
                .ok_or_else(|| anyhow::anyhow!("--zone requires an explicit zone ID"))?,
        )
    } else {
        None
    };
    let language = if zone.is_some() {
        args.next().unwrap_or_else(|| "en".into())
    } else {
        first.unwrap_or_else(|| "en".into())
    };
    anyhow::ensure!(
        args.next().is_none(),
        "usage: uhc-music-details [language] < identity.json"
    );
    let identity: MusicIdentity = if let Some(zone) = &zone {
        now_playing(zone).await?
    } else {
        let mut input = String::new();
        std::io::stdin().take(8193).read_to_string(&mut input)?;
        anyhow::ensure!(input.len() <= 8192, "identity JSON exceeds 8 KiB");
        serde_json::from_str(&input)?
    };
    let client = MusicDetailsClient::from_runtime(unified_hifi_control::config::get_config_dir())?;
    let details = client
        .read(
            &identity,
            &uuid::Uuid::new_v4().simple().to_string(),
            &language,
        )
        .await?;
    if let Some(zone) = &zone {
        anyhow::ensure!(
            now_playing(zone).await? == identity,
            "current item changed while reading details; retry for the new item"
        );
    }
    println!("{}", serde_json::to_string_pretty(&details)?);
    Ok(())
}
