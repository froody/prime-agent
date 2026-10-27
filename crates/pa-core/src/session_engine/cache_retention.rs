//! The live long-cache-retention preference, applied per provider request.

use pa_agent::stream::StreamFn;
use pa_agent::types::CacheRetention;

/// Wrap `stream_fn` so every request (tool continuations and retries included) reads the saved
/// preference: on requests long retention, off leaves the provider default. I/O runs off the
/// async worker, so a settings save applies to a running session's next request.
pub(crate) fn wrap_stream_fn(
    settings: crate::settings::SettingsManager,
    stream_fn: StreamFn,
) -> StreamFn {
    let settings = std::sync::Arc::new(settings);
    std::sync::Arc::new(move |model, context, mut options| {
        let settings = std::sync::Arc::clone(&settings);
        let stream_fn = std::sync::Arc::clone(&stream_fn);
        Box::pin(async move {
            let long =
                tokio::task::spawn_blocking(move || settings.reopen().get_long_cache_retention())
                    .await?;
            if long {
                options.cache_retention = Some(CacheRetention::Long);
            }
            stream_fn(model, context, options).await
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::SettingsManager;
    use pa_agent::stream::{LlmContext, StreamRequestOptions};

    #[tokio::test]
    async fn each_request_reads_the_saved_preference() {
        let tmp = tempfile::tempdir().unwrap();
        let mut settings = SettingsManager::create(tmp.path(), tmp.path().join("agent"));
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = std::sync::Arc::clone(&seen);
        let inner: StreamFn = std::sync::Arc::new(move |_, _, options: StreamRequestOptions| {
            recorded.lock().unwrap().push(options.cache_retention);
            Box::pin(async { Err(anyhow::anyhow!("recorded")) })
        });
        let wrapped = wrap_stream_fn(settings.reopen(), inner);
        for enabled in [false, true, false] {
            settings.set_long_cache_retention(enabled).unwrap();
            let request = wrapped(
                pa_agent::types::Model::unknown(),
                LlmContext::default(),
                StreamRequestOptions::default(),
            );
            assert!(request.await.is_err());
        }
        assert_eq!(
            *seen.lock().unwrap(),
            vec![None, Some(CacheRetention::Long), None]
        );
    }
}
