use std::collections::VecDeque;
use std::time::{Duration, Instant};

use claude_gpt::codex_transport::{UnauthorizedRetryPolicy, ensure_subscription_auth_mode};
use claude_gpt::usage::{UsageCache, UsageSnapshot};
use codex_protocol::auth::AuthMode;

#[test]
fn rejects_api_key_auth_even_when_present() {
    let error = ensure_subscription_auth_mode(AuthMode::ApiKey).unwrap_err();

    assert!(error.to_string().contains("ChatGPT login required"));
}

#[test]
fn accepts_only_interactive_chatgpt_auth() {
    ensure_subscription_auth_mode(AuthMode::Chatgpt).unwrap();

    for mode in [
        AuthMode::ChatgptAuthTokens,
        AuthMode::Headers,
        AuthMode::AgentIdentity,
        AuthMode::PersonalAccessToken,
        AuthMode::BedrockApiKey,
        AuthMode::BedrockAccessKeys,
    ] {
        assert!(
            ensure_subscription_auth_mode(mode).is_err(),
            "accepted {mode}"
        );
    }
}

#[test]
fn retries_one_unauthorized_before_the_stream_starts() {
    let mut statuses = VecDeque::from([401, 200]);
    let mut policy = UnauthorizedRetryPolicy::default();
    let mut attempts = 0;

    loop {
        attempts += 1;
        let status = statuses.pop_front().unwrap();
        if status == 200 {
            break;
        }
        assert!(policy.should_retry(status, false));
    }

    assert_eq!(attempts, 2);
    assert!(!policy.should_retry(401, false));
}

#[test]
fn never_retries_rate_limits_or_started_streams() {
    let mut policy = UnauthorizedRetryPolicy::default();

    assert!(!policy.should_retry(429, false));
    assert!(!policy.should_retry(401, true));
}

#[test]
fn usage_cache_refreshes_no_more_than_once_per_minute() {
    let started_at = Instant::now();
    let mut cache = UsageCache::new(Duration::from_secs(60));
    let snapshot = UsageSnapshot::from_json(serde_json::json!({
        "rate_limits": [{
            "limit_id": "codex",
            "primary": {"used_percent": 25.0, "window_minutes": 300, "resets_at": 42}
        }]
    }))
    .unwrap();

    assert!(cache.should_refresh(started_at));
    cache.record(snapshot.clone(), started_at);
    assert!(!cache.should_refresh(started_at + Duration::from_secs(59)));
    assert!(cache.should_refresh(started_at + Duration::from_secs(60)));
    assert_eq!(cache.snapshot(), Some(&snapshot));
}
