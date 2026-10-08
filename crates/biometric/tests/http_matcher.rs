use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use biometric::{HttpMatcher, MatcherError, Template, TemplateFormat};
use serde_json::{json, Value};
use uuid::Uuid;

type Gallery = Arc<Mutex<HashMap<Uuid, (Uuid, String)>>>;

async fn put_template(
    State(g): State<Gallery>,
    Path(id): Path<Uuid>,
    Json(body): Json<Value>,
) -> StatusCode {
    let subject = Uuid::parse_str(body["subject"].as_str().unwrap()).unwrap();
    let template = body["template"].as_str().unwrap().to_string();
    g.lock().unwrap().insert(id, (subject, template));
    StatusCode::NO_CONTENT
}

async fn delete_template(State(g): State<Gallery>, Path(id): Path<Uuid>) -> StatusCode {
    if g.lock().unwrap().remove(&id).is_some() {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn identify(State(g): State<Gallery>, Json(body): Json<Value>) -> Json<Value> {
    let probe = body["template"].as_str().unwrap();
    let hits: Vec<Value> = g
        .lock()
        .unwrap()
        .iter()
        .map(|(id, (_, t))| {
            let score = if t == probe { 95.0 } else { 5.0 };
            json!({ "enrollment_id": id, "score": score })
        })
        .collect();
    Json(json!({ "hits": hits }))
}

async fn verify(
    State(g): State<Gallery>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    let probe = body["template"].as_str().ok_or(StatusCode::BAD_REQUEST)?;
    let ids: Vec<Uuid> = serde_json::from_value(body["enrollment_ids"].clone())
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let g = g.lock().unwrap();
    let hits: Vec<Value> = ids
        .iter()
        .filter_map(|id| g.get(id).map(|(_, t)| (id, t)))
        .map(|(id, t)| {
            let score = if t == probe { 95.0 } else { 5.0 };
            json!({ "enrollment_id": id, "score": score })
        })
        .collect();
    Ok(Json(json!({ "hits": hits })))
}

async fn spawn_sidecar() -> (String, Gallery) {
    let gallery: Gallery = Arc::new(Mutex::new(HashMap::new()));
    let app = Router::new()
        .route("/health", get(|| async { StatusCode::OK }))
        .route(
            "/v1/templates/{id}",
            put(put_template).delete(delete_template),
        )
        .route("/v1/identify", post(identify))
        .route("/v1/verify", post(verify))
        .route(
            "/broken/v1/verify",
            post(|| async { (StatusCode::OK, "not json") }),
        )
        .route(
            "/down/v1/verify",
            post(|| async { StatusCode::SERVICE_UNAVAILABLE }),
        )
        .with_state(gallery.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), gallery)
}

#[tokio::test]
async fn enroll_identify_revoke_against_the_contract() {
    let (url, gallery) = spawn_sidecar().await;
    let m = HttpMatcher::new(&url, Duration::from_secs(2)).unwrap();
    m.health().await.unwrap();

    let alice = Template::new(TemplateFormat::Raw, vec![1u8; 64]).unwrap();
    let bob = Template::new(TemplateFormat::Raw, vec![2u8; 64]).unwrap();
    let (a_id, b_id) = (Uuid::new_v4(), Uuid::new_v4());
    m.enroll(a_id, Uuid::new_v4(), &alice).await.unwrap();
    m.enroll(b_id, Uuid::new_v4(), &bob).await.unwrap();
    assert_eq!(gallery.lock().unwrap().len(), 2);

    let hits = m.identify(&alice, 5).await.unwrap();
    let best = hits
        .iter()
        .max_by(|x, y| x.score.total_cmp(&y.score))
        .unwrap();
    assert_eq!(best.enrollment_id, a_id);
    assert_eq!(best.score, 95.0);

    m.revoke(a_id).await.unwrap();
    m.revoke(a_id).await.unwrap();
    assert_eq!(gallery.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn verify_scores_only_the_named_enrollments() {
    let (url, _gallery) = spawn_sidecar().await;
    let m = HttpMatcher::new(&url, Duration::from_secs(2)).unwrap();
    let alice = Template::new(TemplateFormat::Raw, vec![4u8; 64]).unwrap();
    let bob = Template::new(TemplateFormat::Raw, vec![5u8; 64]).unwrap();
    let (a_thumb, a_index, b_id) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let a_subject = Uuid::new_v4();
    m.enroll(a_thumb, a_subject, &alice).await.unwrap();
    m.enroll(a_index, a_subject, &bob).await.unwrap();
    m.enroll(b_id, Uuid::new_v4(), &bob).await.unwrap();

    let hits = m.verify(&alice, &[a_thumb, a_index]).await.unwrap();
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|h| h.enrollment_id != b_id));
    let best = hits
        .iter()
        .max_by(|x, y| x.score.total_cmp(&y.score))
        .unwrap();
    assert_eq!((best.enrollment_id, best.score), (a_thumb, 95.0));

    assert!(m.verify(&alice, &[]).await.unwrap().is_empty());
    assert!(m
        .verify(&alice, &[Uuid::new_v4()])
        .await
        .unwrap()
        .is_empty());

    let broken = HttpMatcher::new(&format!("{url}/broken"), Duration::from_secs(2)).unwrap();
    assert!(matches!(
        broken.verify(&alice, &[a_thumb]).await,
        Err(MatcherError::Protocol(_))
    ));
    let down = HttpMatcher::new(&format!("{url}/down"), Duration::from_secs(2)).unwrap();
    assert!(matches!(
        down.verify(&alice, &[a_thumb]).await,
        Err(MatcherError::Unavailable(_))
    ));
}

#[tokio::test]
async fn unreachable_matcher_is_reported_as_unavailable() {
    let m = HttpMatcher::new("http://127.0.0.1:1", Duration::from_millis(500)).unwrap();
    let probe = Template::new(TemplateFormat::Raw, vec![3u8; 64]).unwrap();
    assert!(matches!(
        m.identify(&probe, 5).await,
        Err(MatcherError::Unavailable(_))
    ));
    assert!(matches!(
        m.verify(&probe, &[Uuid::new_v4()]).await,
        Err(MatcherError::Unavailable(_))
    ));
    assert!(matches!(
        HttpMatcher::new("ftp://x", Duration::from_secs(1)),
        Err(MatcherError::Protocol(_))
    ));
}
