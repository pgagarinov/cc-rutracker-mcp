use std::fmt::Write as _;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use regex::Regex;
use rutracker_http::Client;
use rutracker_mirror::driver::{DriverError, SyncDriver};
use rutracker_mirror::engine::SyncOpts;
use rutracker_mirror::Mirror;
use tempfile::{NamedTempFile, TempDir};
use tracing::field::{Field, Visit};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::layer::{Context as LayerContext, SubscriberExt};
use tracing_subscriber::{fmt, Layer, Registry};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

static SYNC_SERIAL: Mutex<()> = Mutex::new(());

const FORUM_ID: &str = "252";

#[derive(Clone, Default)]
struct SharedBuffer {
    bytes: Arc<Mutex<Vec<u8>>>,
}

struct BufferWriter {
    bytes: Arc<Mutex<Vec<u8>>>,
}

struct JsonFileLayer {
    file: Arc<Mutex<std::fs::File>>,
}

#[derive(Default)]
struct JsonVisitor {
    fields: serde_json::Map<String, serde_json::Value>,
}

#[derive(Default, Clone, Copy)]
struct Rfc3339Timer;

impl<'a> fmt::MakeWriter<'a> for SharedBuffer {
    type Writer = BufferWriter;

    fn make_writer(&'a self) -> Self::Writer {
        BufferWriter {
            bytes: self.bytes.clone(),
        }
    }
}

impl Write for BufferWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.bytes.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl FormatTime for Rfc3339Timer {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        write!(w, "{ts}")
    }
}

impl Visit for JsonVisitor {
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), serde_json::Value::Bool(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields.insert(field.name().to_string(), value.into());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields.insert(field.name().to_string(), value.into());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::Number::from_f64(value)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
        );
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::String(value.to_string()),
        );
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::String(format!("{value:?}").trim_matches('"').to_string()),
        );
    }
}

impl<S> Layer<S> for JsonFileLayer
where
    S: tracing::Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: LayerContext<'_, S>) {
        let metadata = event.metadata();
        // Mirror the production `sync_filter()` — only capture sync/driver events.
        if !matches!(
            metadata.target(),
            "rutracker_mirror::sync" | "rutracker_mirror::driver"
        ) {
            return;
        }
        let mut visitor = JsonVisitor::default();
        event.record(&mut visitor);
        visitor.fields.insert(
            "ts".to_string(),
            serde_json::Value::String(
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ),
        );
        visitor.fields.insert(
            "level".to_string(),
            serde_json::Value::String(metadata.level().to_string().to_lowercase()),
        );
        visitor.fields.insert(
            "target".to_string(),
            serde_json::Value::String(metadata.target().to_string()),
        );
        let line = serde_json::to_string(&visitor.fields).unwrap();
        let mut file = self.file.lock().unwrap();
        writeln!(file, "{line}").unwrap();
        file.flush().unwrap();
    }
}

fn serial_guard() -> std::sync::MutexGuard<'static, ()> {
    match SYNC_SERIAL.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn make_client(server: &MockServer) -> Client {
    Client::new(&format!("{}/forum/", server.uri())).unwrap()
}

fn fast_opts(max_topics: usize) -> SyncOpts {
    SyncOpts {
        max_topics,
        max_pages: 100,
        rate_rps: 0.0,
        min_delay_ms: 0,
        max_delay_ms: 0,
        pause_every_n: 0,
        pause_min_secs: 0,
        pause_max_secs: 0,
        rng_seed: Some(7),
        force_full: false,
        transient_retry_delay_ms: 0,
        ..Default::default()
    }
}

fn build_forum_listing_html(forum_id: &str, topic_ids: &[u64]) -> String {
    let mut rows = String::new();
    for (idx, tid) in topic_ids.iter().enumerate() {
        let last_post_id = 1_000_000 + tid;
        let _ = write!(
            rows,
            concat!(
                "<tr class=\"hl-tr\" data-topic_id=\"{tid}\">\n",
                "  <td class=\"vf-col-t-title\"><a class=\"tt-text\" href=\"viewtopic.php?t={tid}\">Topic {tid}</a></td>\n",
                "  <td class=\"u-name-col\"><a>author{idx}</a></td>\n",
                "  <td class=\"tor-size\"><u>100</u> MB</td>\n",
                "  <td><b class=\"seedmed\">5</b></td>\n",
                "  <td class=\"leechmed\">2</td>\n",
                "  <td class=\"vf-col-last-post\"><p>18-Apr-26 12:00</p><p><a href=\"viewtopic.php?p={last_post_id}#{last_post_id}\">link</a></p></td>\n",
                "</tr>\n"
            ),
            tid = tid,
            idx = idx,
            last_post_id = last_post_id,
        );
    }
    let padding = "x".repeat(2048);
    format!(
        r#"<!DOCTYPE html><html><head>
<link rel="canonical" href="https://rutracker.org/forum/viewforum.php?f={forum_id}">
<title>Forum {forum_id} {padding}</title></head><body>
<table class="vf-tor"><tbody>
{rows}
</tbody></table></body></html>"#
    )
}

fn build_topic_html(topic_id: u64) -> String {
    let op_id = 9_000_000 + topic_id;
    format!(
        r##"<!DOCTYPE html><html><head>
<link rel="canonical" href="https://rutracker.org/forum/viewtopic.php?t={topic_id}">
<title>Topic {topic_id}</title></head><body>
<h1 id="topic-title">Test topic {topic_id}</h1>
<a class="magnet-link" href="magnet:?xt=urn:btih:dummy{topic_id}">Magnet</a>
<span id="tor-size-humn">100 MB</span>
<span class="seed"><b>5</b></span>
<span class="leech"><b>2</b></span>
<table>
<tbody id="post_{op_id}">
<tr><td><p class="nick">author</p><a class="p-link small" href="#">18-Apr-26 12:00</a><div class="post_body">Description for {topic_id}</div></td></tr>
</tbody></table></body></html>"##
    )
}

async fn stub_forum_listing(server: &MockServer, forum_id: &str, topic_ids: &[u64]) {
    Mock::given(method("GET"))
        .and(path("/forum/viewforum.php"))
        .and(query_param("f", forum_id))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(build_forum_listing_html(forum_id, topic_ids)),
        )
        .mount(server)
        .await;
}

async fn stub_all_topics(server: &MockServer, topic_ids: &[u64]) {
    for tid in topic_ids {
        Mock::given(method("GET"))
            .and(path("/forum/viewtopic.php"))
            .and(query_param("t", tid.to_string().as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_string(build_topic_html(*tid)))
            .mount(server)
            .await;
    }
}

fn init_json_subscriber(log_path: &std::path::Path) -> tracing::subscriber::DefaultGuard {
    let file = Arc::new(Mutex::new(std::fs::File::create(log_path).unwrap()));
    let human = fmt::layer()
        .compact()
        .with_ansi(false)
        .with_timer(Rfc3339Timer)
        .with_writer(io::sink);
    let json = JsonFileLayer { file };
    let subscriber = Registry::default().with(human).with(json);
    tracing::subscriber::set_default(subscriber)
}

fn init_human_subscriber(buffer: SharedBuffer) -> tracing::subscriber::DefaultGuard {
    let human = fmt::layer()
        .compact()
        .with_ansi(false)
        .with_timer(Rfc3339Timer)
        .with_writer(buffer);
    let subscriber = Registry::default().with(human);
    tracing::subscriber::set_default(subscriber)
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn test_auto_resume_waits_through_cooldown_and_completes() {
    let _serial = serial_guard();
    let server = MockServer::start().await;
    let topic_ids: Vec<u64> = (6_400_001..=6_400_005).collect();

    Mock::given(method("GET"))
        .and(path("/forum/viewforum.php"))
        .and(query_param("f", FORUM_ID))
        .and(query_param("start", "0"))
        .respond_with(ResponseTemplate::new(429))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    stub_forum_listing(&server, FORUM_ID, &topic_ids).await;
    stub_all_topics(&server, &topic_ids).await;

    let td = TempDir::new().unwrap();
    let mut mirror = Mirror::init(td.path()).unwrap();
    let mut driver = SyncDriver::new(&mut mirror, make_client(&server));
    let start = tokio::time::Instant::now();

    let summary = driver
        .run_until_done(
            FORUM_ID,
            SyncOpts {
                cooldown_multiplier: 0.001,
                max_attempts_per_forum: 3,
                ..fast_opts(5)
            },
        )
        .await
        .unwrap();

    assert_eq!(summary.topics_count, 5);
    assert_eq!(summary.attempts, 2);
    assert!(!summary.gave_up);
    assert!(start.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn test_auto_resume_hits_ceiling_on_persistent_429() {
    let _serial = serial_guard();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/forum/viewforum.php"))
        .and(query_param("f", FORUM_ID))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;

    let td = TempDir::new().unwrap();
    let mut mirror = Mirror::init(td.path()).unwrap();
    let mut driver = SyncDriver::new(&mut mirror, make_client(&server));

    let err = driver
        .run_until_done(
            FORUM_ID,
            SyncOpts {
                cooldown_multiplier: 0.001,
                max_attempts_per_forum: 2,
                ..fast_opts(5)
            },
        )
        .await
        .unwrap_err();

    match err {
        DriverError::GaveUp { forum_id, attempts } => {
            assert_eq!(forum_id, FORUM_ID);
            assert_eq!(attempts, 2);
        }
        other => panic!("expected GaveUp, got {other:?}"),
    }
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn test_auto_resume_emits_ndjson_log_file() {
    let _serial = serial_guard();
    let server = MockServer::start().await;
    let topic_ids: Vec<u64> = (6_410_001..=6_410_005).collect();
    stub_forum_listing(&server, FORUM_ID, &topic_ids).await;
    stub_all_topics(&server, &topic_ids).await;

    let log_file = NamedTempFile::new().unwrap();
    let _guard = init_json_subscriber(log_file.path());

    let td = TempDir::new().unwrap();
    let mut mirror = Mirror::init(td.path()).unwrap();
    let mut driver = SyncDriver::new(&mut mirror, make_client(&server));
    let summary = driver
        .run_until_done_all(
            &[FORUM_ID.to_string()],
            SyncOpts {
                max_attempts_per_forum: 2,
                ..fast_opts(5)
            },
        )
        .await
        .unwrap();

    assert_eq!(summary.forums_ok.len(), 1);
    assert!(log_file.path().exists());

    let contents = std::fs::read_to_string(log_file.path()).unwrap();
    let mut saw_forum_start = false;
    let mut saw_sync_complete = false;
    for line in contents.lines().filter(|line| !line.trim().is_empty()) {
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(value.get("ts").is_some(), "missing ts in {line}");
        assert!(value.get("event").is_some(), "missing event in {line}");
        if value.get("event").and_then(|event| event.as_str()) == Some("forum_start") {
            saw_forum_start = true;
        }
        if value.get("event").and_then(|event| event.as_str()) == Some("sync_complete") {
            saw_sync_complete = true;
        }
    }

    assert!(saw_forum_start);
    assert!(saw_sync_complete);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn test_multi_forum_continues_after_one_fails() {
    let _serial = serial_guard();
    let server = MockServer::start().await;
    let ok_topic_ids: Vec<u64> = (6_420_001..=6_420_005).collect();

    Mock::given(method("GET"))
        .and(path("/forum/viewforum.php"))
        .and(query_param("f", "251"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;
    stub_forum_listing(&server, "252", &ok_topic_ids).await;
    stub_all_topics(&server, &ok_topic_ids).await;

    let td = TempDir::new().unwrap();
    let mut mirror = Mirror::init(td.path()).unwrap();
    let mut driver = SyncDriver::new(&mut mirror, make_client(&server));
    let summary = driver
        .run_until_done_all(
            &["251".to_string(), "252".to_string()],
            SyncOpts {
                cooldown_multiplier: 0.001,
                max_attempts_per_forum: 2,
                ..fast_opts(5)
            },
        )
        .await
        .unwrap();

    assert_eq!(summary.forums_ok.len(), 1);
    assert_eq!(summary.forums_failed.len(), 1);
    assert_eq!(summary.forums_ok[0].forum_id, "252");
    assert_eq!(summary.forums_failed[0].forum_id, "251");
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn test_progress_stream_line_delimited() {
    let _serial = serial_guard();
    let server = MockServer::start().await;
    let topic_ids: Vec<u64> = (6_430_001..=6_430_005).collect();
    stub_forum_listing(&server, FORUM_ID, &topic_ids).await;
    stub_all_topics(&server, &topic_ids).await;

    let buffer = SharedBuffer::default();
    let _guard = init_human_subscriber(buffer.clone());

    let td = TempDir::new().unwrap();
    let mut mirror = Mirror::init(td.path()).unwrap();
    let mut driver = SyncDriver::new(&mut mirror, make_client(&server));
    let summary = driver
        .run_until_done_all(
            &[FORUM_ID.to_string()],
            SyncOpts {
                max_attempts_per_forum: 2,
                ..fast_opts(5)
            },
        )
        .await
        .unwrap();

    assert_eq!(summary.forums_ok.len(), 1);

    let output = String::from_utf8(buffer.bytes.lock().unwrap().clone()).unwrap();
    let line_re =
        Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}.*Z\s+(INFO|WARN|ERROR|DEBUG|TRACE)\s+")
            .unwrap();
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        assert!(line_re.is_match(line), "unexpected line format: {line}");
    }
}
