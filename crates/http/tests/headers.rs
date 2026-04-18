use rutracker_http::user_agents::POOL;
use rutracker_http::Client;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ACCEPT: &str =
    "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8";
const ACCEPT_LANGUAGE: &str = "ru-RU,ru;q=0.9,en-US;q=0.8,en;q=0.7";
const ACCEPT_ENCODING: &str = "gzip";
const REFERER: &str = "https://example.org/ref";

fn header_value<'a>(request: &'a wiremock::Request, name: &str) -> Option<&'a str> {
    request.headers.get(name)?.to_str().ok()
}

#[tokio::test]
async fn test_default_headers_include_accept_language_and_encoding() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/forum/tracker.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let client = Client::new(&format!("{}/forum/", server.uri())).unwrap();
    client.get_text("tracker.php", &[]).await.unwrap();

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);

    let request = &requests[0];
    assert_eq!(header_value(request, "accept"), Some(ACCEPT));
    assert_eq!(
        header_value(request, "accept-language"),
        Some(ACCEPT_LANGUAGE)
    );
    assert_eq!(
        header_value(request, "accept-encoding"),
        Some(ACCEPT_ENCODING)
    );
}

#[tokio::test]
async fn test_user_agent_from_pool_and_stable() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/forum/tracker.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let client = Client::new(&format!("{}/forum/", server.uri())).unwrap();
    for _ in 0..5 {
        client.get_text("tracker.php", &[]).await.unwrap();
    }

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 5);

    let user_agent = header_value(&requests[0], "user-agent").unwrap();
    assert!(POOL.contains(&user_agent));
    for request in &requests {
        assert_eq!(header_value(request, "user-agent"), Some(user_agent));
    }
}

#[tokio::test]
async fn test_referer_set_when_provided() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/forum/viewtopic.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let client = Client::new(&format!("{}/forum/", server.uri())).unwrap();
    client
        .get_text_with_referer("viewtopic.php", &[("t", "1")], Some(REFERER))
        .await
        .unwrap();

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(header_value(&requests[0], "referer"), Some(REFERER));
}

#[tokio::test]
async fn test_referer_absent_when_none() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/forum/viewtopic.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let client = Client::new(&format!("{}/forum/", server.uri())).unwrap();
    client
        .get_text_with_referer("viewtopic.php", &[("t", "1")], None)
        .await
        .unwrap();

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].headers.get("referer"), None);
}
