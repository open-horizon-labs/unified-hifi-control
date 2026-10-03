//! Explicit, bounded Cloud context reads associated with aggregator music identity.
use crate::{
    aggregator::ZoneAggregator,
    bus::PrefixedZoneId,
    cloud_connector::music::{MusicDetails, MusicDetailsClient, MusicError, MusicIdentity},
};
use serde::Serialize;
use std::{
    future::Future,
    path::Path,
    pin::Pin,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::sync::Semaphore;

pub trait MusicDetailsReader: Send + Sync {
    fn fetch<'a>(
        &'a self,
        config_dir: &'a Path,
        identity: &'a MusicIdentity,
        item_token: &'a str,
        language: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<MusicDetails, MusicError>> + Send + 'a>>;
}
struct CloudReader;
impl MusicDetailsReader for CloudReader {
    fn fetch<'a>(
        &'a self,
        config_dir: &'a Path,
        identity: &'a MusicIdentity,
        item_token: &'a str,
        language: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<MusicDetails, MusicError>> + Send + 'a>> {
        Box::pin(async move {
            MusicDetailsClient::from_runtime(config_dir)?
                .read(identity, item_token, language)
                .await
        })
    }
}
#[derive(Debug, Serialize)]
pub struct MusicContext {
    pub version: u8,
    pub zone_id: String,
    pub identity: MusicIdentity,
    pub language: String,
    pub details: MusicDetails,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MusicContextError {
    InvalidRequest,
    ZoneNotFound,
    NoMusic,
    MusicChanged,
    CloudNotPaired,
    Unavailable,
}
impl MusicContextError {
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidRequest => "INVALID_REQUEST",
            Self::ZoneNotFound => "ZONE_NOT_FOUND",
            Self::NoMusic => "NO_MUSIC",
            Self::MusicChanged => "MUSIC_CHANGED",
            Self::CloudNotPaired => "CLOUD_NOT_PAIRED",
            Self::Unavailable => "MUSIC_DETAILS_UNAVAILABLE",
        }
    }
    pub fn message(self) -> &'static str {
        match self {
            Self::InvalidRequest => "Invalid music details request",
            Self::ZoneNotFound => "Zone not found",
            Self::NoMusic => "Zone has no usable music identity",
            Self::MusicChanged => "Music changed during the context read",
            Self::CloudNotPaired => "HiPhi Cloud is not paired",
            Self::Unavailable => "Music details are unavailable",
        }
    }
}
pub struct MusicContextService {
    reader: Arc<dyn MusicDetailsReader>,
    permits: Semaphore,
    deadline: Duration,
}
impl MusicContextService {
    pub fn new(
        reader: Arc<dyn MusicDetailsReader>,
        max_concurrent: usize,
        timeout: Duration,
    ) -> Self {
        Self {
            reader,
            permits: Semaphore::new(max_concurrent),
            deadline: timeout,
        }
    }
    pub async fn read_context(
        &self,
        aggregator: &ZoneAggregator,
        config_dir: &Path,
        zone_id: &str,
        language: &str,
    ) -> Result<MusicContext, MusicContextError> {
        if PrefixedZoneId::parse(zone_id).is_none()
            || !zone_id
                .split_once(':')
                .is_some_and(|(_, raw)| !raw.trim().is_empty())
            || zone_id.chars().any(char::is_control)
            || !(2..=12).contains(&language.len())
            || !language
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b == b'-')
        {
            return Err(MusicContextError::InvalidRequest);
        }
        let _permit = self
            .permits
            .try_acquire()
            .map_err(|_| MusicContextError::Unavailable)?;
        tokio::time::timeout(
            self.deadline,
            self.read_inner(aggregator, config_dir, zone_id, language),
        )
        .await
        .map_err(|_| MusicContextError::Unavailable)?
    }
    async fn read_inner(
        &self,
        aggregator: &ZoneAggregator,
        config_dir: &Path,
        zone_id: &str,
        language: &str,
    ) -> Result<MusicContext, MusicContextError> {
        let zone = aggregator
            .get_zone(zone_id)
            .await
            .ok_or(MusicContextError::ZoneNotFound)?;
        let identity =
            usable_identity(zone.now_playing.as_ref()).ok_or(MusicContextError::NoMusic)?;
        let token = uuid::Uuid::new_v4().simple().to_string();
        let result = self
            .reader
            .fetch(config_dir, &identity, &token, language)
            .await;
        // Never hold aggregator state across Cloud IO. Removed/lost music is a change too.
        let after = aggregator
            .get_zone(zone_id)
            .await
            .and_then(|zone| usable_identity(zone.now_playing.as_ref()));
        if after.as_ref() != Some(&identity) {
            return Err(MusicContextError::MusicChanged);
        }
        let details = result.map_err(|error| match error {
            MusicError::NotPaired => MusicContextError::CloudNotPaired,
            _ => MusicContextError::Unavailable,
        })?;
        // Validate the injection boundary as strictly as the production client. Unknown
        // sections remain untouched, including attribution and uncertainty metadata.
        if !details.applies_to(&token)
            || !matches!(
                details.status.as_str(),
                "complete" | "partial" | "ambiguous" | "unavailable"
            )
            || serde_json::to_vec(&details)
                .map_err(|_| MusicContextError::Unavailable)?
                .len()
                > 512 * 1024
        {
            return Err(MusicContextError::Unavailable);
        }
        Ok(MusicContext {
            version: 1,
            zone_id: zone_id.into(),
            identity,
            language: language.into(),
            details,
        })
    }
}
fn usable_identity(item: Option<&crate::bus::NowPlaying>) -> Option<MusicIdentity> {
    let identity = MusicIdentity::from_now_playing(item?);
    let text = [&identity.artist, &identity.title, &identity.album];
    if text.iter().all(|v| v.is_none())
        || text
            .iter()
            .filter_map(|v| v.as_ref())
            .any(|v| v.len() > 512 || v.chars().any(char::is_control))
    {
        return None;
    }
    Some(identity)
}

pub fn shared_service() -> &'static MusicContextService {
    static SERVICE: OnceLock<MusicContextService> = OnceLock::new();
    SERVICE
        .get_or_init(|| MusicContextService::new(Arc::new(CloudReader), 4, Duration::from_secs(10)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::bus::{BusEvent, EventBus, NowPlaying, PlaybackState, Zone};
    use tokio::sync::Notify;
    struct Delayed {
        entered: Notify,
        release: Notify,
    }
    impl MusicDetailsReader for Delayed {
        fn fetch<'a>(
            &'a self,
            _: &'a Path,
            _: &'a MusicIdentity,
            token: &'a str,
            _: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<MusicDetails, MusicError>> + Send + 'a>> {
            Box::pin(async move {
                self.entered.notify_one();
                self.release.notified().await;
                Ok(MusicDetails {
                    version: 1,
                    item_token: token.into(),
                    status: "partial".into(),
                    sections: Default::default(),
                })
            })
        }
    }
    fn zone(title: &str) -> Zone {
        Zone {
            zone_id: "roon:test".into(),
            zone_name: "Test".into(),
            state: PlaybackState::Playing,
            volume_control: None,
            now_playing: Some(NowPlaying {
                title: title.into(),
                artist: "Artist".into(),
                album: "Album".into(),
                image_key: None,
                seek_position: None,
                duration: Some(120.),
                metadata: None,
                repeat_mode: None,
                shuffle: None,
            }),
            source: "roon".into(),
            is_controllable: true,
            is_seekable: true,
            last_updated: 0,
            is_play_allowed: true,
            is_pause_allowed: true,
            is_next_allowed: true,
            is_previous_allowed: true,
        }
    }
    async fn setup() -> (
        Arc<EventBus>,
        Arc<ZoneAggregator>,
        tokio::task::JoinHandle<()>,
    ) {
        let bus = Arc::new(EventBus::new(32));
        let agg = Arc::new(ZoneAggregator::new(bus.clone()));
        let (tx, rx) = tokio::sync::oneshot::channel();
        let a = agg.clone();
        let task = tokio::spawn(async move { a.run_with_ready(tx).await });
        rx.await.unwrap();
        publish(&bus, &agg, "A").await;
        (bus, agg, task)
    }
    async fn publish(bus: &EventBus, agg: &ZoneAggregator, title: &str) {
        bus.publish(BusEvent::ZoneDiscovered { zone: zone(title) });
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if agg
                    .get_now_playing("roon:test")
                    .await
                    .is_some_and(|n| n.title == title)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn delayed_result_cannot_attach_to_changed_music() {
        let (bus, agg, task) = setup().await;
        let reader = Arc::new(Delayed {
            entered: Notify::new(),
            release: Notify::new(),
        });
        let service = MusicContextService::new(reader.clone(), 1, Duration::from_secs(1));
        let request = service.read_context(&agg, Path::new("/unused"), "roon:test", "en");
        tokio::pin!(request);
        tokio::select! { _=reader.entered.notified()=>{}, result=&mut request=>panic!("unexpected {result:?}") }
        publish(&bus, &agg, "B").await;
        reader.release.notify_one();
        assert_eq!(request.await.unwrap_err(), MusicContextError::MusicChanged);
        task.abort();
    }
    struct Reply {
        failure: Option<u16>,
        malformed: bool,
        oversized: bool,
    }
    impl MusicDetailsReader for Reply {
        fn fetch<'a>(
            &'a self,
            _: &'a Path,
            _: &'a MusicIdentity,
            token: &'a str,
            _: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<MusicDetails, MusicError>> + Send + 'a>> {
            Box::pin(async move {
                if let Some(status) = self.failure {
                    return Err(if status == 0 {
                        MusicError::NotPaired
                    } else {
                        MusicError::Refused(status)
                    });
                }
                let mut sections = serde_json::Map::new();
                if self.oversized {
                    sections.insert(
                        "extra".into(),
                        serde_json::Value::String("x".repeat(512 * 1024)),
                    );
                }
                Ok(MusicDetails {
                    version: 1,
                    item_token: if self.malformed {
                        "wrong".into()
                    } else {
                        token.into()
                    },
                    status: "partial".into(),
                    sections,
                })
            })
        }
    }
    fn immediate(failure: Option<u16>, malformed: bool, oversized: bool) -> MusicContextService {
        MusicContextService::new(
            Arc::new(Reply {
                failure,
                malformed,
                oversized,
            }),
            1,
            Duration::from_secs(1),
        )
    }
    #[tokio::test]
    async fn validates_caller_and_empty_identity_without_cloud() {
        let (bus, agg, task) = setup().await;
        assert_eq!(
            immediate(None, false, false)
                .read_context(&agg, Path::new("/unused"), "test", "en")
                .await
                .unwrap_err(),
            MusicContextError::InvalidRequest
        );
        assert_eq!(
            immediate(None, false, false)
                .read_context(&agg, Path::new("/unused"), "roon:test", "EN")
                .await
                .unwrap_err(),
            MusicContextError::InvalidRequest
        );
        let mut empty = zone("");
        let item = empty.now_playing.as_mut().unwrap();
        item.artist.clear();
        item.album.clear();
        bus.publish(BusEvent::ZoneDiscovered { zone: empty });
        while agg.get_now_playing("roon:test").await.unwrap().title != "" {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            immediate(None, false, false)
                .read_context(&agg, Path::new("/unused"), "roon:test", "en")
                .await
                .unwrap_err(),
            MusicContextError::NoMusic
        );
        task.abort();
    }
    #[tokio::test]
    async fn sanitizes_failures_and_validates_reader_reply() {
        let (_, agg, task) = setup().await;
        assert_eq!(
            immediate(Some(0), false, false)
                .read_context(&agg, Path::new("/unused"), "roon:test", "en")
                .await
                .unwrap_err(),
            MusicContextError::CloudNotPaired
        );
        for service in [
            immediate(Some(429), false, false),
            immediate(Some(503), false, false),
            immediate(Some(401), false, false),
            immediate(None, true, false),
            immediate(None, false, true),
        ] {
            assert_eq!(
                service
                    .read_context(&agg, Path::new("/unused"), "roon:test", "en")
                    .await
                    .unwrap_err(),
                MusicContextError::Unavailable
            );
        }
        task.abort();
    }
    #[tokio::test]
    async fn shared_capacity_and_deadline_bound_slow_reads() {
        let (_, agg, task) = setup().await;
        let reader = Arc::new(Delayed {
            entered: Notify::new(),
            release: Notify::new(),
        });
        let service = MusicContextService::new(reader.clone(), 1, Duration::from_millis(30));
        let first = service.read_context(&agg, Path::new("/unused"), "roon:test", "en");
        tokio::pin!(first);
        tokio::select! {_=reader.entered.notified()=>{},r=&mut first=>panic!("unexpected {r:?}")}
        let overloaded = tokio::time::timeout(
            Duration::from_millis(100),
            service.read_context(&agg, Path::new("/unused"), "roon:test", "en"),
        )
        .await;
        assert_eq!(
            overloaded
                .expect("overload must return without waiting")
                .unwrap_err(),
            MusicContextError::Unavailable
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), first)
                .await
                .expect("fixed deadline")
                .unwrap_err(),
            MusicContextError::Unavailable
        );
        // Deadline cancellation releases the shared permit for the next explicit read.
        reader.release.notify_one();
        assert!(service
            .read_context(&agg, Path::new("/unused"), "roon:test", "en")
            .await
            .is_ok());
        task.abort();
    }

    #[tokio::test]
    async fn removed_zone_or_lost_music_refuses_late_reply() {
        for remove in [true, false] {
            let (bus, agg, task) = setup().await;
            let reader = Arc::new(Delayed {
                entered: Notify::new(),
                release: Notify::new(),
            });
            let service = MusicContextService::new(reader.clone(), 1, Duration::from_secs(1));
            let request = service.read_context(&agg, Path::new("/unused"), "roon:test", "en");
            tokio::pin!(request);
            tokio::select! {_=reader.entered.notified()=>{},r=&mut request=>panic!("unexpected {r:?}")}
            if remove {
                bus.publish(BusEvent::ZoneRemoved {
                    zone_id: PrefixedZoneId::roon("test"),
                });
            } else {
                let mut z = zone("A");
                z.now_playing = None;
                bus.publish(BusEvent::ZoneDiscovered { zone: z });
            }
            while agg.get_now_playing("roon:test").await.is_some() {
                tokio::task::yield_now().await;
            }
            reader.release.notify_one();
            assert_eq!(request.await.unwrap_err(), MusicContextError::MusicChanged);
            task.abort();
        }
    }
    #[tokio::test]
    async fn identity_not_occurrence_and_seek_changes_do_not_refuse() {
        let (bus, agg, task) = setup().await;
        let reader = Arc::new(Delayed {
            entered: Notify::new(),
            release: Notify::new(),
        });
        let service = MusicContextService::new(reader.clone(), 1, Duration::from_secs(1));
        let request = service.read_context(&agg, Path::new("/unused"), "roon:test", "en");
        tokio::pin!(request);
        tokio::select! {_=reader.entered.notified()=>{},r=&mut request=>panic!("unexpected {r:?}")}
        publish(&bus, &agg, "B").await;
        publish(&bus, &agg, "A").await;
        let mut changed = zone("A");
        changed.now_playing.as_mut().unwrap().seek_position = Some(88.);
        changed.state = PlaybackState::Paused;
        bus.publish(BusEvent::ZoneDiscovered { zone: changed });
        while agg
            .get_now_playing("roon:test")
            .await
            .unwrap()
            .seek_position
            != Some(88.)
        {
            tokio::task::yield_now().await;
        }
        reader.release.notify_one();
        assert_eq!(request.await.unwrap().identity.title.as_deref(), Some("A"));
        task.abort();
    }
    #[tokio::test]
    async fn explicit_unknown_zone_and_grammar_valid_language() {
        let (_, agg, task) = setup().await;
        assert_eq!(
            immediate(None, false, false)
                .read_context(&agg, Path::new("/unused"), "roon:missing", "en")
                .await
                .unwrap_err(),
            MusicContextError::ZoneNotFound
        );
        assert!(immediate(None, false, false)
            .read_context(&agg, Path::new("/unused"), "roon:test", "unsupported")
            .await
            .is_ok());
        task.abort();
    }
    struct InvalidDetails {
        version: u8,
        status: &'static str,
    }
    impl MusicDetailsReader for InvalidDetails {
        fn fetch<'a>(
            &'a self,
            _: &'a Path,
            _: &'a MusicIdentity,
            token: &'a str,
            _: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<MusicDetails, MusicError>> + Send + 'a>> {
            Box::pin(async move {
                Ok(MusicDetails {
                    version: self.version,
                    item_token: token.into(),
                    status: self.status.into(),
                    sections: Default::default(),
                })
            })
        }
    }
    #[tokio::test]
    async fn unknown_version_or_status_is_not_a_cloud_answer() {
        let (_, agg, task) = setup().await;
        for (version, status) in [(2, "partial"), (1, "invented")] {
            let service = MusicContextService::new(
                Arc::new(InvalidDetails { version, status }),
                1,
                Duration::from_secs(1),
            );
            assert_eq!(
                service
                    .read_context(&agg, Path::new("/unused"), "roon:test", "en")
                    .await
                    .unwrap_err(),
                MusicContextError::Unavailable
            );
        }
        task.abort();
    }
}
