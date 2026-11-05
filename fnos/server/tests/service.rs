use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use switchhosts_fnos::{
    api::{self, Auth},
    model::{Document, Entry},
    service::Service,
    target::{self, Target},
};
use tower::ServiceExt;

const TOKEN: &str = "development-test-token-with-32-characters";

async fn setup(hosts: &str) -> (tempfile::TempDir, std::sync::Arc<Service>) {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("hosts");
    std::fs::write(&target, hosts).unwrap();
    let service = Service::open(dir.path().join("data"), Target::DevelopmentFile(target))
        .await
        .unwrap();
    (dir, service)
}
fn entry(id: &str, content: &str, enabled: bool) -> Entry {
    Entry {
        id: id.into(),
        title: id.into(),
        content: content.into(),
        enabled,
        ..Default::default()
    }
}
async fn call(
    router: Router,
    method: &str,
    path: &str,
    body: Value,
    authorized: bool,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(format!("/app/switchhosts/api/{path}"))
        .header("content-type", "application/json")
        .header("x-switchhosts-request", "1");
    if authorized {
        builder = builder.header("authorization", format!("Bearer {TOKEN}"));
    }
    let response = router
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn api_requires_authentication_and_rejects_browser_cross_site_writes() {
    let (_dir, service) = setup("127.0.0.1 localhost\n").await;
    let app = api::router(service.clone(), Auth::DevelopmentToken(TOKEN.into()));
    assert_eq!(
        call(app.clone(), "GET", "state", Value::Null, false)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(app.clone(), "GET", "state", Value::Null, true).await.0,
        StatusCode::OK
    );
    let forged = Request::builder()
        .uri("/app/switchhosts/api/state")
        .header("x-trim-isadmin", "true")
        .header("x-trim-userid", "1000")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(forged).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    for extra in [None, Some("cross-site")] {
        let mut request = Request::builder()
            .method("POST")
            .uri("/app/switchhosts/api/reconcile")
            .header("authorization", format!("Bearer {TOKEN}"));
        if let Some(site) = extra {
            request = request
                .header("sec-fetch-site", site)
                .header("x-switchhosts-request", "1");
        }
        assert_eq!(
            app.clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    let response = app
        .oneshot(
            Request::builder()
                .uri("/app/switchhosts/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .contains("frame-ancestors 'self'"));
    let gateway = api::router(service, Auth::Gateway);
    for (admin, status) in [("false", StatusCode::FORBIDDEN), ("true", StatusCode::OK)] {
        let request = Request::builder()
            .uri("/app/switchhosts/api/state")
            .header("x-trim-isadmin", admin)
            .header("x-trim-userid", "1000")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            gateway.clone().oneshot(request).await.unwrap().status(),
            status
        );
    }
}

#[tokio::test]
async fn switching_preserves_unmanaged_content_and_rejects_stale_edits() {
    let hosts = "127.0.0.1 localhost\n# --- SWITCHHOSTS_CONTENT_START ---\n10.0.0.9 old.test\n# --- SWITCHHOSTS_CONTENT_END ---\n192.168.1.5 external.test\n";
    let (_dir, service) = setup(hosts).await;
    service
        .save_entry(0, entry("local", "\u{feff}10.0.0.1 dev.test\r\n", true))
        .await
        .unwrap();
    let applied = service.target.read().await.unwrap();
    assert!(
        applied.contains("external.test")
            && applied.contains("dev.test")
            && !applied.contains("old.test")
            && !applied.contains('\u{feff}')
    );
    let app = api::router(service.clone(), Auth::DevelopmentToken(TOKEN.into()));
    assert_eq!(
        call(
            app,
            "POST",
            "entry",
            json!({"revision":0,"entry":entry("local","stale",true)}),
            true
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(service.target.read().await.unwrap(), applied);
    service
        .save_entry(1, entry("local", "10.0.0.1 dev.test", false))
        .await
        .unwrap();
    let disabled = service.target.read().await.unwrap();
    assert!(disabled.contains("external.test") && !disabled.contains("dev.test"));
    let doc = service.document().await;
    let original = doc
        .history
        .iter()
        .find(|s| s.content == hosts)
        .unwrap()
        .id
        .clone();
    service.restore(doc.revision, &original).await.unwrap();
    assert_eq!(service.target.read().await.unwrap(), hosts);
    assert!(service.document().await.entries.iter().all(|e| !e.enabled));
}

#[tokio::test]
async fn deleting_an_active_entry_updates_system_hosts() {
    let (_dir, service) = setup("127.0.0.1 localhost\n").await;
    service
        .save_entry(0, entry("one", "10.0.0.1 one.test", true))
        .await
        .unwrap();
    service
        .save_entry(1, entry("two", "10.0.0.2 two.test", true))
        .await
        .unwrap();
    service.delete(2, "one").await.unwrap();
    let hosts = service.target.read().await.unwrap();
    assert!(hosts.contains("two.test") && !hosts.contains("one.test"));
}

#[tokio::test]
async fn background_subscription_refresh_applies_without_a_browser_and_retains_cache_on_failure() {
    use axum::routing::get;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/hosts", get(|| async { "10.0.0.8 background.test\n" }))
                .route("/fail", get(|| async { StatusCode::SERVICE_UNAVAILABLE })),
        )
        .await
        .unwrap();
    });
    let (_dir, service) = setup("127.0.0.1 localhost\n").await;
    let mut remote = entry("remote", "10.0.0.7 cached.test", true);
    remote.url = format!("http://{address}/hosts");
    remote.interval_seconds = 60;
    service.save_entry(0, remote).await.unwrap();
    service.scan_once().await;
    assert!(service
        .target
        .read()
        .await
        .unwrap()
        .contains("background.test"));
    let mut doc = service.document().await;
    assert!(doc.entries[0].last_success > 0);
    doc.entries[0].url = format!("http://{address}/fail");
    service
        .save_entry(doc.revision, doc.entries[0].clone())
        .await
        .unwrap();
    let before = service.target.read().await.unwrap();
    assert!(service.refresh("remote").await.is_err());
    assert_eq!(service.target.read().await.unwrap(), before);
    let doc = service.document().await;
    assert!(!doc.entries[0].last_error.is_empty() && doc.entries[0].last_attempt > 0);
    let revision = doc.revision;
    service.scan_once().await;
    assert_eq!(
        service.document().await.revision,
        revision,
        "failed refresh must respect interval"
    );
    server.abort();
}

#[tokio::test]
async fn failed_remote_download_never_replaces_last_good_content() {
    let (_dir, service) = setup("127.0.0.1 localhost\n").await;
    let mut remote = entry("remote", "10.0.0.7 cached.test", true);
    remote.url = "http://127.0.0.1:1/hosts".into();
    service.save_entry(0, remote).await.unwrap();
    assert!(service.refresh("remote").await.is_err());
    assert!(service.document().await.entries[0]
        .content
        .contains("cached.test"));
    assert!(service.target.read().await.unwrap().contains("cached.test"));
}

#[tokio::test]
async fn pending_write_is_reconciled_after_restart() {
    for written in [false, true] {
        let (dir, service) = setup("before\n").await;
        let next = Document {
            revision: 1,
            ..Default::default()
        };
        let journal = json!({"before":"before\n", "after":"after\n", "next":next});
        std::fs::write(
            dir.path().join("data/pending.json"),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
        if written {
            std::fs::write(dir.path().join("hosts"), "after\n").unwrap();
        }
        drop(service);
        let reopened = Service::open(
            dir.path().join("data"),
            Target::DevelopmentFile(dir.path().join("hosts")),
        )
        .await
        .unwrap();
        assert_eq!(reopened.document().await.revision, u64::from(written));
        assert!(!reopened.recovery_required());
    }
}

#[tokio::test]
async fn external_changes_during_recovery_are_preserved_and_block_mutations() {
    let (dir, service) = setup("before\n").await;
    service
        .save_entry(0, entry("local", "10.0.0.1 local.test", false))
        .await
        .unwrap();
    let doc = service.document().await;
    std::fs::write(
        dir.path().join("data/pending.json"),
        serde_json::to_vec(&json!({"before":"before\n","after":"after\n","next":doc})).unwrap(),
    )
    .unwrap();
    std::fs::write(dir.path().join("hosts"), "external\n").unwrap();
    assert!(service.reconcile().await.is_err());
    assert!(service
        .save_entry(1, entry("local", "10.0.0.2 changed.test", true))
        .await
        .is_err());
    service.keep_current(1).await.unwrap();
    assert_eq!(service.target.read().await.unwrap(), "external\n");
    assert!(!service.recovery_required());
    assert!(service.document().await.entries.iter().all(|e| !e.enabled));
}

#[tokio::test]
async fn imports_are_inactive_and_cannot_destroy_active_configuration() {
    let (_dir, service) = setup("before\n").await;
    service
        .save_entry(0, entry("local", "10.0.0.1 local.test", true))
        .await
        .unwrap();
    let mut imported = Document::default();
    imported
        .entries
        .push(entry("imported", "10.0.0.9 imported.test", true));
    assert!(service.import(1, imported.clone()).await.is_err());
    service
        .save_entry(1, entry("local", "10.0.0.1 local.test", false))
        .await
        .unwrap();
    let hosts = service.target.read().await.unwrap();
    service.import(2, imported).await.unwrap();
    assert_eq!(service.target.read().await.unwrap(), hosts);
    assert!(!service.document().await.entries[0].enabled);
}

#[tokio::test]
async fn invalid_data_and_second_instances_are_refused() {
    let (dir, service) = setup("before\n").await;
    assert!(Service::open(
        dir.path().join("data"),
        Target::DevelopmentFile(dir.path().join("hosts"))
    )
    .await
    .is_err());
    for invalid in [
        entry("../etc", "valid", true),
        entry("nul", "bad\0text", true),
        entry("large", &"x".repeat(4 * 1024 * 1024 + 1), true),
    ] {
        assert!(service.save_entry(0, invalid).await.is_err());
    }
    assert_eq!(service.target.read().await.unwrap(), "before\n");
    drop(service);
    std::fs::write(dir.path().join("data/state.json"), "broken").unwrap();
    assert!(Service::open(
        dir.path().join("data"),
        Target::DevelopmentFile(dir.path().join("hosts"))
    )
    .await
    .is_err());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("data/state.json")).unwrap(),
        "broken"
    );
}

#[test]
fn compare_and_swap_does_not_overwrite_external_changes_or_file_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hosts");
    std::fs::write(&path, "external").unwrap();
    assert!(target::compare_and_write(&path, "stale", "replacement").is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "external");
    target::compare_and_write(&path, "external", "ok").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "ok");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(target::compare_and_write(&link, "ok", "bad").is_err());
    }
}
