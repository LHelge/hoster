use reqwest::Url;
use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::*;

const REPO: &str = "/repos/LHelge/hoster";

async fn server() -> (MockServer, GithubReleases) {
    let server = MockServer::start().await;
    let repo: Url = format!("{}{REPO}", server.uri()).parse().unwrap();
    let source = GithubReleases::with(repo, UrlPolicy::insecure(&["127.0.0.1"])).unwrap();
    (server, source)
}

fn release_json(server: &MockServer, tag: &str, prerelease: bool) -> serde_json::Value {
    json!({
        "tag_name": tag,
        "name": tag,
        "body": "## Features\n- things",
        "draft": false,
        "prerelease": prerelease,
        "assets": [{
            "name": "hoster-x86_64-unknown-linux-musl",
            "browser_download_url": format!("{}/download/{tag}/hoster-x86_64-unknown-linux-musl", server.uri()),
            "size": 5,
        }, {
            "name": "SHA256SUMS",
            "browser_download_url": format!("{}/download/{tag}/SHA256SUMS", server.uri()),
            "size": 100,
        }],
    })
}

fn asset(server: &MockServer, file: &str, size: Option<u64>) -> Asset {
    Asset {
        name: file.into(),
        url: format!("{}/download/{file}", server.uri()).parse().unwrap(),
        size,
    }
}

#[test]
fn the_github_policy_allows_only_https_on_release_hosts() {
    let policy = UrlPolicy::github();
    for ok in [
        "https://api.github.com/repos/LHelge/hoster/releases/latest",
        "https://github.com/LHelge/hoster/releases/download/v0.1.0/SHA256SUMS",
        "https://objects.githubusercontent.com/github-production-release-asset/1",
        "https://release-assets.githubusercontent.com/github-production-release-asset/1",
    ] {
        assert!(policy.permits(&ok.parse().unwrap()), "{ok}");
    }
    for bad in [
        "http://github.com/LHelge/hoster/releases/download/v0.1.0/SHA256SUMS",
        "https://evil.example/hoster",
        "https://github.com.evil.example/x",
        "https://user:pw@github.com/x",
        "ftp://github.com/x",
    ] {
        assert!(!policy.permits(&bad.parse().unwrap()), "{bad}");
    }
}

#[tokio::test]
async fn latest_reads_version_notes_and_assets() {
    let (server, source) = server().await;
    Mock::given(method("GET"))
        .and(path(format!("{REPO}/releases/latest")))
        .and(header("accept", "application/vnd.github+json"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(release_json(&server, "v0.2.0", false)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let release = source.latest().await.unwrap();
    assert_eq!(release.version, "0.2.0".parse().unwrap());
    assert_eq!(release.notes, "## Features\n- things");
    let names: Vec<_> = release.assets.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["hoster-x86_64-unknown-linux-musl", "SHA256SUMS"]);
    assert_eq!(release.asset("SHA256SUMS").unwrap().size, Some(100));

    // The User-Agent names hoster and its version, nothing about the host or its owner.
    let requests: Vec<Request> = server.received_requests().await.unwrap();
    let agent = requests[0]
        .headers
        .get("user-agent")
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(
        agent,
        concat!(
            "hoster/",
            env!("CARGO_PKG_VERSION"),
            " (+https://github.com/LHelge/hoster)"
        )
    );
    assert!(requests[0].headers.get("authorization").is_none());
}

#[tokio::test]
async fn latest_never_returns_a_pre_release() {
    let (server, source) = server().await;
    Mock::given(path(format!("{REPO}/releases/latest")))
        .respond_with(ResponseTemplate::new(200).set_body_json(release_json(
            &server,
            "v0.3.0-rc.1",
            true,
        )))
        .mount(&server)
        .await;
    let err = source.latest().await.unwrap_err();
    assert!(matches!(err, SourceError::NotStable(_)), "{err}");
}

#[tokio::test]
async fn a_tagged_release_may_be_a_pre_release() {
    let (server, source) = server().await;
    Mock::given(path(format!("{REPO}/releases/tags/v0.3.0-rc.1")))
        .respond_with(ResponseTemplate::new(200).set_body_json(release_json(
            &server,
            "v0.3.0-rc.1",
            true,
        )))
        .mount(&server)
        .await;
    let version = "0.3.0-rc.1".parse().unwrap();
    assert_eq!(source.release(&version).await.unwrap().version, version);
}

#[tokio::test]
async fn missing_releases_and_rate_limits_are_named() {
    let (server, source) = server().await;
    Mock::given(path(format!("{REPO}/releases/latest")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(path(format!("{REPO}/releases/tags/v9.9.9")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(path(format!("{REPO}/releases/tags/v0.1.0")))
        .respond_with(ResponseTemplate::new(403).insert_header("x-ratelimit-remaining", "0"))
        .mount(&server)
        .await;
    assert!(matches!(source.latest().await, Err(SourceError::NoRelease)));
    let err = source.release(&"9.9.9".parse().unwrap()).await.unwrap_err();
    assert!(matches!(err, SourceError::UnknownRelease(_)), "{err}");
    let err = source.release(&"0.1.0".parse().unwrap()).await.unwrap_err();
    assert!(matches!(err, SourceError::RateLimited { .. }), "{err}");
}

#[tokio::test]
async fn a_tag_that_is_not_a_version_is_malformed() {
    let (server, source) = server().await;
    Mock::given(path(format!("{REPO}/releases/latest")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(release_json(&server, "nightly", false)),
        )
        .mount(&server)
        .await;
    assert!(matches!(
        source.latest().await,
        Err(SourceError::Malformed(_))
    ));
}

#[tokio::test]
async fn downloads_follow_allowed_redirects_and_write_the_file() {
    let (server, source) = server().await;
    Mock::given(path("/download/SHA256SUMS"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/storage/sums", server.uri())),
        )
        .mount(&server)
        .await;
    Mock::given(path("/storage/sums"))
        .and(header("accept", "application/octet-stream"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"abc  hoster\n".to_vec()))
        .mount(&server)
        .await;
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("sums");
    source
        .download(&asset(&server, "SHA256SUMS", None), 1024, &dest)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), b"abc  hoster\n");
}

#[tokio::test]
async fn redirects_off_the_allowed_hosts_are_refused() {
    let (server, source) = server().await;
    // `localhost` is the same server, but not on the allow-list.
    let port = server.address().port();
    Mock::given(path("/download/hoster"))
        .respond_with(ResponseTemplate::new(302).insert_header(
            "location",
            format!("http://localhost:{port}/storage/hoster"),
        ))
        .mount(&server)
        .await;
    Mock::given(path("/storage/hoster"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"evil".to_vec()))
        .expect(0)
        .mount(&server)
        .await;
    let dir = TempDir::new().unwrap();
    let err = source
        .download(&asset(&server, "hoster", None), 1024, &dir.path().join("x"))
        .await
        .unwrap_err();
    assert!(matches!(err, SourceError::UntrustedUrl { .. }), "{err}");

    // An asset URL off the allow-list is never requested at all.
    let foreign = Asset {
        name: "hoster".into(),
        url: format!("http://localhost:{port}/storage/hoster")
            .parse()
            .unwrap(),
        size: None,
    };
    let err = source
        .download(&foreign, 1024, &dir.path().join("y"))
        .await
        .unwrap_err();
    assert!(matches!(err, SourceError::UntrustedUrl { .. }), "{err}");
}

#[tokio::test]
async fn downloads_are_size_bounded() {
    let (server, source) = server().await;
    Mock::given(path("/download/big"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 4096]))
        .mount(&server)
        .await;
    let dir = TempDir::new().unwrap();
    // The size the release announces is checked before the request, the response's own
    // length before its body is read.
    let err = source
        .download(
            &asset(&server, "big", Some(4096)),
            1024,
            &dir.path().join("a"),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, SourceError::TooLarge { limit: 1024, .. }),
        "{err}"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
    let err = source
        .download(&asset(&server, "big", None), 1024, &dir.path().join("b"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, SourceError::TooLarge { limit: 1024, .. }),
        "{err}"
    );
}

#[tokio::test]
async fn an_unanswered_download_times_out() {
    let server = MockServer::start().await;
    Mock::given(path("/download/slow"))
        .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(5)))
        .mount(&server)
        .await;
    let repo: Url = format!("{}{REPO}", server.uri()).parse().unwrap();
    let source = GithubReleases::with(repo, UrlPolicy::insecure(&["127.0.0.1"]))
        .unwrap()
        .with_download_timeout(std::time::Duration::from_millis(200));
    let dir = TempDir::new().unwrap();
    let err = source
        .download(&asset(&server, "slow", None), 1024, &dir.path().join("s"))
        .await
        .unwrap_err();
    assert!(matches!(err, SourceError::Timeout { .. }), "{err}");
}

/// Against the real GitHub API: `TEST_HOSTER_GITHUB_LIVE=1 cargo test github_live`.
#[tokio::test]
async fn github_live_latest_release() {
    if std::env::var("TEST_HOSTER_GITHUB_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set TEST_HOSTER_GITHUB_LIVE=1 to query api.github.com");
        return;
    }
    let source = GithubReleases::new().unwrap();
    let release = source.latest().await.unwrap();
    assert!(!release.version.is_prerelease());
    assert!(release.asset(Release::CHECKSUMS).is_some(), "{release:?}");
    assert!(
        release
            .asset(&Release::binary_name("x86_64-unknown-linux-musl"))
            .is_some(),
        "{release:?}"
    );
    assert_eq!(
        source.release(&release.version).await.unwrap().version,
        release.version
    );
}
