//! One request-scoped dispatch lifetime, shared across nested control paths.
//! This scopes a Tokio task, not global configuration or a persisted authority.
use std::future::Future;
use std::time::Duration;
use tokio::time::Instant;

tokio::task_local! { static DISPATCH_BY: Instant; }
pub const LOCAL_DISPATCH_BUDGET: Duration = Duration::from_secs(3);

pub fn deadline() -> Instant {
    DISPATCH_BY
        .try_with(|deadline| *deadline)
        .unwrap_or_else(|_| Instant::now() + LOCAL_DISPATCH_BUDGET)
}

/// Nesting may shorten a caller's lifetime, never renew it.
pub async fn until<F: Future>(by: Instant, future: F) -> F::Output {
    let by = DISPATCH_BY.try_with(|outer| by.min(*outer)).unwrap_or(by);
    DISPATCH_BY.scope(by, future).await
}
pub async fn local<F: Future>(future: F) -> F::Output {
    until(deadline(), future).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn nested_local_work_cannot_renew_authority_and_context_does_not_leak() {
        let by = Instant::now() + Duration::from_millis(20);
        until(by, async {
            local(async {
                until(Instant::now() + Duration::from_secs(30), async {
                    assert_eq!(deadline(), by);
                    tokio::time::advance(Duration::from_millis(21)).await;
                    assert!(Instant::now() >= deadline());
                })
                .await;
            })
            .await;
        })
        .await;
        assert!(deadline() > Instant::now());
    }
}

#[derive(Debug, thiserror::Error)]
#[error("command expired before native dispatch")]
pub(crate) struct DispatchExpired;
pub(crate) const EXPIRED_MESSAGE: &str = "command expired before native dispatch";
