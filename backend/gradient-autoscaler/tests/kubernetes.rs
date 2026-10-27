mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradient_autoscaler::catch_up::catch_up;
use gradient_autoscaler::config::WorkerCapabilities;
use gradient_autoscaler::github::oidc::OidcVerifier;
use gradient_autoscaler::http::{App, router};
use gradient_autoscaler::kubernetes::client::KubernetesClient;
use gradient_autoscaler::scaler::scale;
use gradient_autoscaler::setup::setup;
use gradient_autoscaler::state::{Launch, LaunchState};
use serde_json::{Value, json};
use std::sync::Arc;
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;
use url::Url;
use uuid::Uuid;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, Request as Received, ResponseTemplate};

const PODS: &str = "/api/v1/namespaces/ci/pods";
const SECRETS: &str = "/api/v1/namespaces/ci/secrets";

fn envelope(message: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "error": false, "message": message }))
}

async fn mount(
    server: &MockServer,
    verb: &str,
    route: &str,
    response: ResponseTemplate,
    times: Option<u64>,
) {
    let mock = Mock::given(method(verb))
        .and(path(route))
        .respond_with(response);
    match times {
        Some(times) => mock.expect(times).mount(server).await,
        None => mock.mount(server).await,
    }
}

async fn demand(server: &MockServer, builds: Value) {
    let body = json!({ "builds": builds, "network": 0, "secrets": 0, "eval": 0 });
    mount(
        server,
        "GET",
        "/api/v1/teams/ops/workers/demand",
        envelope(body),
        None,
    )
    .await;
}

async fn received(server: &MockServer, verb: &str, route: &str) -> Vec<Received> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() == verb && r.url.path() == route)
        .collect()
}

fn json_of(request: &Received) -> Value {
    serde_json::from_slice(&request.body).unwrap()
}

fn pod(name: &str, phase: &str, waiting: Option<&str>) -> ResponseTemplate {
    let state = match waiting {
        Some(reason) => json!({ "waiting": { "reason": reason } }),
        None => json!({ "running": {} }),
    };
    ResponseTemplate::new(200).set_body_json(json!({
        "metadata": { "name": name, "uid": "uid-1" },
        "status": { "phase": phase, "containerStatuses": [{ "state": state }] },
    }))
}

async fn open_launch(
    svc: &gradient_autoscaler::service::ConnectionService,
    state: LaunchState,
) -> Launch {
    open_launch_since(svc, state, OffsetDateTime::now_utc()).await
}

async fn open_launch_since(
    svc: &gradient_autoscaler::service::ConnectionService,
    state: LaunchState,
    dispatched_at: OffsetDateTime,
) -> Launch {
    let id = Uuid::new_v4();
    let launch = Launch {
        id,
        state,
        worker_id: Some(Uuid::new_v4()),
        dispatched_at,
        ..Launch::new("ops", &format!("gradient-worker-{id}"), "x86")
    };
    svc.state.insert_launch(&launch).await.unwrap();
    launch
}

#[tokio::test]
async fn scale_starts_a_pod_whose_secret_holds_the_registered_token() {
    let server = MockServer::start().await;
    let mut config = common::kubernetes();
    config.pools.get_mut("x86").unwrap().pod_options = serde_json::from_value(json!({
        "volumes": [{"name": "fetch-auth", "secret": {"secretName": "worker-fetch-auth"}}],
        "volumeMounts": [{"name": "fetch-auth", "mountPath": "/creds", "readOnly": true}],
        "env": [{"name": "FETCH_MODE", "valueFrom": {"configMapKeyRef": {"name": "worker-config", "key": "mode"}}}]
    })).unwrap();
    let mut fixture = common::kubernetes_service_with_config(&server.uri(), config).await;
    Arc::get_mut(&mut fixture.svc)
        .unwrap()
        .conn
        .worker_capabilities = WorkerCapabilities {
        eval: false,
        secrets: false,
        exec: true,
        ..Default::default()
    };
    let svc = &fixture.svc;
    demand(
        &server,
        json!([{ "architecture": "x86_64-linux", "required_features": ["kvm"], "build_count": 3 }]),
    )
    .await;
    let team = Uuid::from_u128(5);
    let registered = envelope(json!({ "team": team, "token": "worker-token" }));
    mount(
        &server,
        "POST",
        "/api/v1/teams/ops/workers",
        registered,
        Some(1),
    )
    .await;
    Mock::given(method("POST"))
        .and(path(PODS))
        .and(header("Authorization", "Bearer service-account-token"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "metadata": { "name": "created", "uid": "pod-uid", "labels": { "gradient.wavelens.io/connection": "ops" } },
        })))
        .expect(1)
        .mount(&server)
        .await;
    mount(
        &server,
        "POST",
        SECRETS,
        ResponseTemplate::new(201),
        Some(1),
    )
    .await;

    scale(svc).await.unwrap();

    let launches = svc.state.open_launches("ops").await.unwrap();
    assert_eq!(launches.len(), 1);
    let launch = &launches[0];
    assert_eq!(launch.label, "x86");
    assert!(!launch.swapped);

    let registration = json_of(&received(&server, "POST", "/api/v1/teams/ops/workers").await[0]);
    let worker_id = registration["worker_id"].as_str().unwrap();
    assert_eq!(launch.worker_id.unwrap().to_string(), worker_id);
    assert_eq!(registration["enable_build"], true);
    assert_eq!(registration["enable_network"], true);
    assert_eq!(registration["enable_eval"], false);
    assert_eq!(registration["enable_secrets"], false);
    assert_eq!(registration["enable_exec"], true);

    let manifest = json_of(&received(&server, "POST", PODS).await[0]);
    assert_eq!(manifest["metadata"]["name"], launch.external_id);
    assert_eq!(
        manifest["metadata"]["labels"]["gradient.wavelens.io/launch"],
        launch.id.to_string()
    );
    let env = manifest["spec"]["containers"][0]["env"].as_array().unwrap();
    assert!(
        env.iter()
            .any(|var| var["name"] == "GRADIENT_WORKER_ID" && var["value"] == worker_id)
    );
    assert!(!manifest.to_string().contains("worker-token"));
    for (role, enabled) in [
        ("BUILD", "true"),
        ("NETWORK", "true"),
        ("EVAL", "false"),
        ("SECRETS", "false"),
        ("EXEC", "true"),
    ] {
        assert!(env.iter().any(|var| {
            var["name"] == format!("GRADIENT_WORKER_CAPABILITIES_{role}") && var["value"] == enabled
        }));
    }
    assert!(
        manifest["spec"]["volumes"].as_array().unwrap().contains(
            &json!({"name": "fetch-auth", "secret": {"secretName": "worker-fetch-auth"}})
        )
    );
    assert!(
        manifest["spec"]["containers"][0]["volumeMounts"]
            .as_array()
            .unwrap()
            .contains(&json!({"name": "fetch-auth", "mountPath": "/creds", "readOnly": true}))
    );
    assert!(env.iter().any(|var| var
        == &json!({
            "name": "FETCH_MODE",
            "valueFrom": {"configMapKeyRef": {"name": "worker-config", "key": "mode"}}
        })));

    let secret = json_of(&received(&server, "POST", SECRETS).await[0]);
    assert_eq!(
        secret["stringData"]["peers"],
        format!("{team}:worker-token\n")
    );
    assert_eq!(secret["metadata"]["ownerReferences"][0]["uid"], "pod-uid");
}

#[tokio::test]
async fn rejected_pod_unregisters_its_worker_and_records_no_launch() {
    let server = MockServer::start().await;
    let fixture = common::kubernetes_service(&server.uri()).await;
    demand(
        &server,
        json!([{ "architecture": "aarch64-linux", "required_features": [], "build_count": 1 }]),
    )
    .await;
    let registered = envelope(json!({ "team": Uuid::nil(), "token": "worker-token" }));
    mount(
        &server,
        "POST",
        "/api/v1/teams/ops/workers",
        registered,
        Some(1),
    )
    .await;
    mount(&server, "POST", PODS, ResponseTemplate::new(403), Some(1)).await;
    mount(
        &server,
        "POST",
        SECRETS,
        ResponseTemplate::new(201),
        Some(0),
    )
    .await;
    Mock::given(method("DELETE"))
        .and(wiremock::matchers::path_regex(
            "^/api/v1/teams/ops/workers/.+",
        ))
        .respond_with(envelope(json!("deleted")))
        .expect(1)
        .mount(&server)
        .await;

    scale(&fixture.svc).await.unwrap();

    assert!(
        fixture
            .svc
            .state
            .open_launches("ops")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn pending_pod_is_removed_once_demand_is_gone() {
    let server = MockServer::start().await;
    let fixture = common::kubernetes_service(&server.uri()).await;
    let svc = &fixture.svc;
    demand(&server, json!([])).await;
    let launch = open_launch(svc, LaunchState::Queued).await;
    let pod_path = format!("{PODS}/{}", launch.external_id);
    mount(
        &server,
        "DELETE",
        &pod_path,
        ResponseTemplate::new(200),
        Some(1),
    )
    .await;
    let worker_path = format!("/api/v1/teams/ops/workers/{}", launch.worker_id.unwrap());
    mount(
        &server,
        "DELETE",
        &worker_path,
        envelope(json!("deleted")),
        Some(1),
    )
    .await;

    scale(svc).await.unwrap();

    let stored = svc.state.launch("ops", launch.id).await.unwrap().unwrap();
    assert_eq!(stored.state, LaunchState::Cancelled);
}

#[tokio::test]
async fn catch_up_follows_pod_phases() {
    let server = MockServer::start().await;
    let fixture = common::kubernetes_service(&server.uri()).await;
    let svc = &fixture.svc;
    demand(&server, json!([])).await;
    let finished = open_launch(svc, LaunchState::Connected).await;
    let broken_image = open_launch(svc, LaunchState::Queued).await;
    let running = open_launch(svc, LaunchState::Running).await;
    let hour_ago = OffsetDateTime::now_utc() - Duration::hours(1);
    let never_scheduled = open_launch_since(svc, LaunchState::Queued, hour_ago).await;
    let open_pods: Vec<Value> = [&finished, &broken_image, &running, &never_scheduled]
        .iter()
        .map(|launch| json!({ "metadata": { "name": launch.external_id, "labels": { "gradient.wavelens.io/launch": launch.id.to_string() } } }))
        .collect();
    Mock::given(method("GET"))
        .and(path(PODS))
        .and(query_param(
            "labelSelector",
            "gradient.wavelens.io/connection=ops",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "items": open_pods })))
        .mount(&server)
        .await;
    mount(
        &server,
        "GET",
        "/api/v1/teams/ops/workers",
        envelope(json!([])),
        None,
    )
    .await;
    for (launch, phase, waiting) in [
        (&finished, "Succeeded", None),
        (&broken_image, "Pending", Some("ImagePullBackOff")),
        (&running, "Running", None),
        (&never_scheduled, "Pending", Some("ContainerCreating")),
    ] {
        let pod_path = format!("{PODS}/{}", launch.external_id);
        mount(
            &server,
            "GET",
            &pod_path,
            pod(&launch.external_id, phase, waiting),
            None,
        )
        .await;
        let removals = if launch.id == running.id { 0 } else { 1 };
        mount(
            &server,
            "DELETE",
            &pod_path,
            ResponseTemplate::new(200),
            Some(removals),
        )
        .await;
        let worker_path = format!("/api/v1/teams/ops/workers/{}", launch.worker_id.unwrap());
        mount(
            &server,
            "DELETE",
            &worker_path,
            envelope(json!("deleted")),
            Some(removals),
        )
        .await;
    }

    catch_up(svc).await;

    let state = |id| async move { svc.state.launch("ops", id).await.unwrap().unwrap().state };
    assert_eq!(state(finished.id).await, LaunchState::Completed);
    assert_eq!(state(broken_image.id).await, LaunchState::Failed);
    assert_eq!(state(running.id).await, LaunchState::Running);
    assert_eq!(state(never_scheduled.id).await, LaunchState::Failed);
}

#[tokio::test]
async fn api_server_path_prefix_is_kept() {
    let server = MockServer::start().await;
    let mut token = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut token, b"t").unwrap();
    let api = Url::parse(&format!("{}/k8s/clusters/c-1/", server.uri())).unwrap();
    let client = KubernetesClient::with_api_url(api, token.path().to_owned(), "ci");
    Mock::given(method("GET"))
        .and(path("/k8s/clusters/c-1/api/v1/namespaces/ci/pods/p"))
        .respond_with(pod("p", "Running", None))
        .expect(1)
        .mount(&server)
        .await;

    assert!(client.pod("p").await.unwrap().is_some());
}

#[tokio::test]
async fn setup_deletes_pods_without_an_open_launch() {
    let server = MockServer::start().await;
    let fixture = common::kubernetes_service(&server.uri()).await;
    let svc = &fixture.svc;
    let kept = open_launch(svc, LaunchState::Running).await;
    let pods = json!({ "items": [
        { "metadata": { "name": kept.external_id, "labels": { "gradient.wavelens.io/launch": kept.id.to_string() } } },
        { "metadata": { "name": "gradient-worker-lost", "labels": { "gradient.wavelens.io/launch": Uuid::new_v4().to_string() } } },
    ] });
    mount(
        &server,
        "GET",
        PODS,
        ResponseTemplate::new(200).set_body_json(pods),
        None,
    )
    .await;
    mount(
        &server,
        "DELETE",
        &format!("{PODS}/gradient-worker-lost"),
        ResponseTemplate::new(200),
        Some(1),
    )
    .await;
    mount(
        &server,
        "DELETE",
        &format!("{PODS}/{}", kept.external_id),
        ResponseTemplate::new(200),
        Some(0),
    )
    .await;

    setup(svc).await;
}

#[tokio::test]
async fn github_endpoints_do_not_exist_for_a_kubernetes_connection() {
    let server = MockServer::start().await;
    let fixture = common::kubernetes_service(&server.uri()).await;
    let app = Arc::new(App::new(
        vec![fixture.svc.clone()],
        OidcVerifier::new("https://issuer.invalid"),
    ));

    for endpoint in ["github", "swap"] {
        let request = Request::post(format!("/c/ops/{endpoint}"))
            .header("Content-Type", "application/json")
            .body(Body::from(
                r#"{"token":"t","launch_id":"00000000-0000-0000-0000-000000000000"}"#,
            ))
            .unwrap();
        let status = router(app.clone()).oneshot(request).await.unwrap().status();
        assert_eq!(status, StatusCode::NOT_FOUND, "{endpoint}");
    }
}
