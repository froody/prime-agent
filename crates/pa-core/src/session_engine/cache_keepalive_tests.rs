use super::*;
use pa_agent::stream::event_stream;
use pa_agent::types::UsageCost;

fn reply(stop_reason: StopReason, cached: u64) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: "openai-completions".into(),
        provider: "litellm".into(),
        model: "claude".into(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: Usage {
            cache_read: cached,
            total_tokens: cached,
            ..Usage::zero()
        },
        stop_reason,
        stop_reason_raw: None,
        error_message: None,
        timestamp: 0,
    }
}

fn opus() -> Model {
    let mut model = Model::unknown();
    model.cost = UsageCost {
        input: 4.0,
        output: 20.0,
        cache_read: 0.2,
        cache_write: 5.0,
        total: 0.0,
    };
    model
}

/// Every request the provider sees, and the replies it answers with in order.
struct Provider {
    seen: Arc<Mutex<Vec<(LlmContext, StreamRequestOptions)>>>,
    stream_fn: StreamFn,
}

fn provider(replies: Vec<AssistantMessage>) -> Provider {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let replies = Arc::new(Mutex::new(replies.into_iter()));
    let recorded = Arc::clone(&seen);
    let stream_fn: StreamFn = Arc::new(move |_, context, options| {
        recorded.lock().unwrap().push((context, options));
        let reply = replies.lock().unwrap().next().expect("a scripted reply");
        Box::pin(async move {
            let (handle, stream) = event_stream();
            handle.end(Some(reply));
            Ok(Box::new(stream) as Box<dyn ModelStream>)
        })
    });
    Provider { seen, stream_fn }
}

fn bills() -> (RecordPing, Arc<Mutex<Vec<ModelUsage>>>) {
    let billed = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&billed);
    (
        Arc::new(move |usage| sink.lock().unwrap().push(usage)),
        billed,
    )
}

async fn send(stream_fn: &StreamFn, model: Model, context: LlmContext) {
    let options = StreamRequestOptions {
        reasoning: pa_agent::types::ThinkingLevel::High,
        ..StreamRequestOptions::default()
    };
    let mut stream = stream_fn(model, context, options).await.unwrap();
    stream.result().await.unwrap();
}

/// Let spawned tasks run, then advance paused time past the ping delay.
async fn idle(duration: Duration) {
    tokio::task::yield_now().await;
    tokio::time::sleep(duration).await;
    tokio::task::yield_now().await;
}

fn context(text: &str) -> LlmContext {
    LlmContext {
        system_prompt: Some("system".into()),
        messages: vec![pa_agent::types::Message::User(
            pa_agent::types::UserMessage {
                content: pa_agent::types::UserContent::Text(text.into()),
                timestamp: 0,
            },
        )],
        tools: Vec::new(),
    }
}

#[tokio::test(start_paused = true)]
async fn an_idle_end_of_turn_reply_pings_once_with_long_retention() {
    let provider = provider(vec![
        reply(StopReason::ToolUse, 50_000),
        reply(StopReason::Stop, 50_000),
        reply(StopReason::Stop, 50_001),
    ]);
    let (record, billed) = bills();
    let wrapped = wrap_stream_fn(Arc::new(|| true), record, Arc::clone(&provider.stream_fn));

    send(&wrapped, opus(), context("tool turn")).await;
    idle(PING_DELAY * 2).await;
    assert_eq!(
        provider.seen.lock().unwrap().len(),
        1,
        "no ping after a tool call"
    );

    send(&wrapped, opus(), context("answer")).await;
    idle(PING_DELAY * 4).await;

    let seen = provider.seen.lock().unwrap();
    assert_eq!(seen.len(), 3, "exactly one ping");
    let (ping_context, ping_options) = &seen[2];
    assert_eq!(
        serde_json::to_value(ping_context).unwrap(),
        serde_json::to_value(context("answer")).unwrap()
    );
    assert_eq!(
        (
            ping_options.cache_retention,
            ping_options.max_tokens,
            ping_options.reasoning
        ),
        (
            Some(CacheRetention::Long),
            Some(1),
            pa_agent::types::ThinkingLevel::High
        )
    );
    assert_eq!(
        billed
            .lock()
            .unwrap()
            .iter()
            .map(|usage| usage.source.as_str())
            .collect::<Vec<_>>(),
        vec![PING_USAGE_SOURCE]
    );
}

#[tokio::test(start_paused = true)]
async fn no_ping_when_it_cannot_pay_or_the_user_returns() {
    let mut no_cache_price = opus();
    no_cache_price.cost.cache_write = 0.0;
    let cases: Vec<(&str, Model, bool, u64)> = vec![
        ("disabled", opus(), false, 50_000),
        ("small prompt", opus(), true, MIN_CACHED_TOKENS - 1),
        ("no cache-write price", no_cache_price, true, 50_000),
    ];
    for (case, model, enabled, cached) in cases {
        let provider = provider(vec![
            reply(StopReason::Stop, cached),
            reply(StopReason::Stop, cached),
        ]);
        let (record, _) = bills();
        let wrapped = wrap_stream_fn(
            Arc::new(move || enabled),
            record,
            Arc::clone(&provider.stream_fn),
        );
        send(&wrapped, model, context("answer")).await;
        idle(PING_DELAY * 2).await;
        assert_eq!(provider.seen.lock().unwrap().len(), 1, "{case}");
    }

    let provider = provider(vec![
        reply(StopReason::Stop, 50_000),
        reply(StopReason::ToolUse, 50_000),
    ]);
    let (record, _) = bills();
    let wrapped = wrap_stream_fn(Arc::new(|| true), record, Arc::clone(&provider.stream_fn));
    send(&wrapped, opus(), context("answer")).await;
    idle(PING_DELAY / 2).await;
    send(&wrapped, opus(), context("user is back")).await;
    idle(PING_DELAY * 2).await;
    assert_eq!(
        provider.seen.lock().unwrap().len(),
        2,
        "a new request cancels the ping"
    );
}
