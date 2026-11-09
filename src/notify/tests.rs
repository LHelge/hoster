use std::time::Duration;

use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

const KEY: &str = "re_test_supersecret";

fn message() -> Message {
    Message {
        from: "hoster@example.com".parse().unwrap(),
        to: "ops@example.org".parse().unwrap(),
        subject: "[srv-01] Deploy of web failed".to_owned(),
        text: "Deploy of web failed".to_owned(),
        html: "<p>Deploy of web failed</p>".to_owned(),
        idempotency_key: "event-42".parse().unwrap(),
    }
}

fn provider(server: &MockServer) -> Arc<dyn Provider> {
    ProviderKind::Resend
        .build_at(SecretValue::from(KEY), server.uri().parse().unwrap())
        .unwrap()
}

/// Answers every `POST /emails` with `response` and returns what the provider made of it.
async fn send_with(response: ResponseTemplate) -> Result<Delivery, DeliveryError> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/emails"))
        .respond_with(response)
        .mount(&server)
        .await;
    provider(&server).send(&message()).await
}

fn resend_error(status: u16, name: &str, message: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(serde_json::json!({
        "statusCode": status,
        "name": name,
        "message": message,
    }))
}

/// Neither the error's `Display` nor its `Debug` may hold the API key.
fn assert_no_key(error: &DeliveryError) {
    assert!(!error.to_string().contains(KEY), "{error}");
    assert!(!format!("{error:?}").contains(KEY), "{error:?}");
}

#[tokio::test]
async fn success_posts_the_message_with_key_user_agent_and_idempotency_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/emails"))
        .and(header("authorization", format!("Bearer {KEY}").as_str()))
        .and(header("idempotency-key", "event-42"))
        .and(header("content-type", "application/json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "4ef9"})))
        .expect(1)
        .mount(&server)
        .await;

    let delivery = provider(&server).send(&message()).await.unwrap();
    assert_eq!(delivery.id.as_deref(), Some("4ef9"));

    let requests = server.received_requests().await.unwrap();
    let request = &requests[0];
    let agent = request.headers["user-agent"].to_str().unwrap();
    assert_eq!(agent, concat!("hoster/", env!("CARGO_PKG_VERSION")));
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(
        body,
        serde_json::json!({
            "from": "hoster@example.com",
            "to": ["ops@example.org"],
            "subject": "[srv-01] Deploy of web failed",
            "text": "Deploy of web failed",
            "html": "<p>Deploy of web failed</p>",
        })
    );
}

#[tokio::test]
async fn a_base_url_with_a_path_keeps_it() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/emails"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "x"})))
        .expect(1)
        .mount(&server)
        .await;
    let base = format!("{}/v1", server.uri()).parse().unwrap();
    let provider = ProviderKind::Resend
        .build_at(SecretValue::from(KEY), base)
        .unwrap();
    provider.send(&message()).await.unwrap();
}

#[tokio::test]
async fn a_key_a_header_cannot_carry_fails_locally_without_a_status() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let provider = ProviderKind::Resend
        .build_at(
            SecretValue::from(format!("{KEY}\n")),
            server.uri().parse().unwrap(),
        )
        .unwrap();
    let error = provider.send(&message()).await.unwrap_err();
    assert!(matches!(error, DeliveryError::Client { .. }), "{error:?}");
    assert!(!error.is_retryable());
    assert_eq!(error.status(), None);
    assert_no_key(&error);
}

#[tokio::test]
async fn success_without_a_json_body_is_still_a_delivery() {
    let delivery = send_with(ResponseTemplate::new(200).set_body_string("ok"))
        .await
        .unwrap();
    assert_eq!(delivery, Delivery::default());
}

#[tokio::test]
async fn unauthorized_is_permanent_and_carries_the_message() {
    let error = send_with(resend_error(401, "missing_api_key", "Missing API key"))
        .await
        .unwrap_err();
    assert!(!error.is_retryable());
    assert_eq!(error.status(), Some(StatusCode::UNAUTHORIZED));
    assert_eq!(error.code(), Some("missing_api_key"));
    assert_eq!(error.provider_message(), Some("Missing API key"));
    assert_eq!(
        error.to_string(),
        "Resend answered 401 Unauthorized (missing_api_key): Missing API key"
    );
    assert_no_key(&error);
}

#[tokio::test]
async fn forbidden_unverified_domain_is_permanent() {
    let text = "The example.com domain is not verified. Please, add and verify your domain.";
    let error = send_with(resend_error(403, "validation_error", text))
        .await
        .unwrap_err();
    assert!(!error.is_retryable());
    assert_eq!(error.retry_after(), None);
    assert_eq!(error.status(), Some(StatusCode::FORBIDDEN));
    assert!(error.to_string().contains(text), "{error}");
    assert_no_key(&error);
}

#[tokio::test]
async fn validation_error_is_permanent() {
    let error = send_with(resend_error(
        422,
        "invalid_parameter",
        "Invalid `to` field.",
    ))
    .await
    .unwrap_err();
    assert!(!error.is_retryable());
    assert_eq!(error.status(), Some(StatusCode::UNPROCESSABLE_ENTITY));
    assert_eq!(error.code(), Some("invalid_parameter"));
}

#[tokio::test]
async fn rate_limit_is_retryable_after_the_retry_after_header() {
    let error = send_with(
        resend_error(429, "rate_limit_exceeded", "Too many requests.")
            .insert_header("retry-after", "2"),
    )
    .await
    .unwrap_err();
    assert!(error.is_retryable());
    assert_eq!(error.retry_after(), Some(Duration::from_secs(2)));
    assert_eq!(error.code(), Some("rate_limit_exceeded"));
    assert!(error.to_string().ends_with("; retry after 2s"), "{error}");
}

#[tokio::test]
async fn rate_limit_without_retry_after_is_retryable_without_a_delay() {
    let error = send_with(resend_error(
        429,
        "rate_limit_exceeded",
        "Too many requests.",
    ))
    .await
    .unwrap_err();
    assert!(error.is_retryable());
    assert_eq!(error.retry_after(), None);
}

#[tokio::test]
async fn daily_and_monthly_quota_are_permanent() {
    for name in ["daily_quota_exceeded", "monthly_quota_exceeded"] {
        let error = send_with(
            resend_error(429, name, "You have reached your quota.")
                .insert_header("retry-after", "3600"),
        )
        .await
        .unwrap_err();
        assert!(!error.is_retryable(), "{name}");
        assert_eq!(error.retry_after(), None, "{name}");
        assert_eq!(error.code(), Some(name));
    }
}

#[tokio::test]
async fn server_error_is_retryable() {
    let error = send_with(resend_error(500, "application_error", "Unexpected error."))
        .await
        .unwrap_err();
    assert!(error.is_retryable());
    assert_eq!(error.status(), Some(StatusCode::INTERNAL_SERVER_ERROR));
    assert_eq!(error.retry_after(), None);
    assert_no_key(&error);
}

#[tokio::test]
async fn server_error_honours_retry_after() {
    let error = send_with(
        resend_error(503, "application_error", "Try again later.")
            .insert_header("retry-after", "7"),
    )
    .await
    .unwrap_err();
    assert!(error.is_retryable());
    assert_eq!(error.retry_after(), Some(Duration::from_secs(7)));
    assert!(error.to_string().ends_with("; retry after 7s"), "{error}");
}

#[tokio::test]
async fn a_non_json_answer_is_classified_by_status() {
    let page = "<html><body>502 Bad Gateway</body></html>";
    let error = send_with(ResponseTemplate::new(502).set_body_string(page))
        .await
        .unwrap_err();
    assert!(error.is_retryable());
    assert_eq!(error.code(), None);
    assert_eq!(error.provider_message(), None);
    assert_eq!(error.to_string(), "Resend answered 502 Bad Gateway");

    let error = send_with(ResponseTemplate::new(403).set_body_string("Forbidden"))
        .await
        .unwrap_err();
    assert!(!error.is_retryable());
    let error = send_with(ResponseTemplate::new(429).set_body_string("slow down"))
        .await
        .unwrap_err();
    assert!(error.is_retryable());
}

#[tokio::test]
async fn concurrent_idempotent_request_is_retryable_and_a_reused_key_is_not() {
    let error = send_with(resend_error(
        409,
        "concurrent_idempotent_requests",
        "Same idempotency key in flight.",
    ))
    .await
    .unwrap_err();
    assert!(error.is_retryable());
    let error = send_with(resend_error(
        409,
        "invalid_idempotent_request",
        "Same key, different payload.",
    ))
    .await
    .unwrap_err();
    assert!(!error.is_retryable());
}

#[tokio::test]
async fn a_long_provider_message_is_cut() {
    let long = "x".repeat(2000);
    let error = send_with(resend_error(422, "validation_error", &long))
        .await
        .unwrap_err();
    assert_eq!(error.provider_message().unwrap().len(), 500);
}

#[tokio::test]
async fn a_timeout_is_retryable() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .mount(&server)
        .await;
    let provider = ProviderKind::Resend
        .build_with(
            SecretValue::from(KEY),
            server.uri().parse().unwrap(),
            Duration::from_millis(200),
        )
        .unwrap();
    let error = provider.send(&message()).await.unwrap_err();
    assert!(matches!(error, DeliveryError::Timeout { .. }), "{error:?}");
    assert!(error.is_retryable());
    assert_eq!(error.to_string(), "Resend did not answer within 200ms");
}

#[tokio::test]
async fn a_refused_connection_is_retryable_and_hides_the_key() {
    // Bind and drop a listener so the port is very likely closed.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let provider = ProviderKind::Resend
        .build_at(
            SecretValue::from(KEY),
            format!("http://127.0.0.1:{port}/").parse().unwrap(),
        )
        .unwrap();
    let error = provider.send(&message()).await.unwrap_err();
    assert!(matches!(error, DeliveryError::Network { .. }), "{error:?}");
    assert!(error.is_retryable());
    assert!(
        error.to_string().starts_with("cannot reach Resend"),
        "{error}"
    );
    assert_no_key(&error);
}

#[test]
fn the_api_key_never_shows_in_debug() {
    let provider = ProviderKind::Resend.build(SecretValue::from(KEY)).unwrap();
    let debug = format!("{provider:?}");
    assert!(!debug.contains(KEY), "{debug}");
    assert!(debug.contains("<redacted>"), "{debug}");
    assert!(debug.contains("https://api.resend.com/emails"), "{debug}");
}

#[test]
fn provider_kind_round_trips_through_its_stored_name() {
    for kind in ProviderKind::ALL {
        assert_eq!(kind.to_string().parse::<ProviderKind>(), Ok(kind));
    }
    assert_eq!("resend".parse(), Ok(ProviderKind::Resend));
    assert_eq!(ProviderKind::Resend.label(), "Resend");
    assert!("mailjet".parse::<ProviderKind>().is_err());
    assert!("Resend".parse::<ProviderKind>().is_err());
}

#[test]
fn mail_addresses_are_checked() {
    let address: MailAddress = "  ops@example.org ".parse().unwrap();
    assert_eq!(address.to_string(), "ops@example.org");
    assert_eq!(address.as_ref(), "ops@example.org");
    assert!(
        "first.last+tag@sub.example.co"
            .parse::<MailAddress>()
            .is_ok()
    );

    for (input, error) in [
        ("", InvalidMailAddress::Empty),
        ("   ", InvalidMailAddress::Empty),
        ("ops", InvalidMailAddress::Shape),
        ("@example.org", InvalidMailAddress::Shape),
        ("ops@", InvalidMailAddress::Shape),
        ("a@b@example.org", InvalidMailAddress::Shape),
        ("ops @example.org", InvalidMailAddress::Character),
        ("ops@example.org,evil@x.io", InvalidMailAddress::Character),
        ("Ops <ops@example.org>", InvalidMailAddress::Character),
        (
            "ops@example.org\r\nBcc: x@y.z",
            InvalidMailAddress::Character,
        ),
        (
            "ops@localhost",
            InvalidMailAddress::Domain("localhost".into()),
        ),
        (
            "ops@example..org",
            InvalidMailAddress::Domain("example..org".into()),
        ),
        (
            "ops@-example.org",
            InvalidMailAddress::Domain("-example.org".into()),
        ),
    ] {
        assert_eq!(input.parse::<MailAddress>(), Err(error), "{input:?}");
    }
    let long = format!("{}@example.org", "a".repeat(250));
    assert_eq!(
        long.parse::<MailAddress>(),
        Err(InvalidMailAddress::TooLong)
    );
}

#[test]
fn idempotency_keys_are_header_safe() {
    assert!("notify/42-reminder".parse::<IdempotencyKey>().is_ok());
    assert_eq!(
        "".parse::<IdempotencyKey>(),
        Err(InvalidIdempotencyKey::Empty)
    );
    assert_eq!(
        "a b".parse::<IdempotencyKey>(),
        Err(InvalidIdempotencyKey::Character)
    );
    assert_eq!(
        "é".parse::<IdempotencyKey>(),
        Err(InvalidIdempotencyKey::Character)
    );
    assert_eq!(
        "k".repeat(257).parse::<IdempotencyKey>(),
        Err(InvalidIdempotencyKey::TooLong)
    );
    assert!("k".repeat(256).parse::<IdempotencyKey>().is_ok());
}
