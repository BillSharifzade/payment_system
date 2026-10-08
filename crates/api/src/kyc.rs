use axum::extract::{Multipart, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use storage::StorageError;
use uuid::Uuid;

use crate::admin::audit;
use crate::session::{AdminUser, AuthUser};
use crate::{ApiError, ApiResult, AppState};

#[derive(Deserialize)]
pub struct SubmitKycRequest {
    requested_level: i16,
    full_name: String,
    document_type: String,
    document_ref: String,
}

#[derive(Serialize)]
pub struct KycSubmissionResponse {
    id: Uuid,
    status: String,
    requested_level: i16,
}

#[derive(Serialize)]
pub struct KycStatusResponse {
    kyc_level: i16,
    latest_submission: Option<KycSubmissionResponse>,
}

pub(crate) async fn user_kyc_level(state: &AppState, user_id: Uuid) -> ApiResult<i16> {
    let level: Option<i16> = sqlx::query_scalar("SELECT kyc_level FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(state.ledger.pool())
        .await
        .map_err(StorageError::from)?;
    level.ok_or_else(|| ApiError::Unauthorized("unknown user".to_string()))
}

pub(crate) async fn ensure_kyc(state: &AppState, user_id: Uuid, min_level: i16) -> ApiResult<()> {
    if user_kyc_level(state, user_id).await? >= min_level {
        Ok(())
    } else {
        Err(ApiError::KycRequired(format!(
            "this action requires KYC level {min_level}"
        )))
    }
}

pub async fn submit_kyc(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Json(req): Json<SubmitKycRequest>,
) -> ApiResult<(StatusCode, Json<KycSubmissionResponse>)> {
    if !(1..=2).contains(&req.requested_level) {
        return Err(ApiError::BadRequest(
            "requested_level must be 1 or 2".to_string(),
        ));
    }
    if req.full_name.trim().is_empty() || req.document_ref.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "full_name and document_ref are required".to_string(),
        ));
    }

    let id = Uuid::now_v7();
    let result = sqlx::query(
        "INSERT INTO kyc_submissions
           (id, user_id, requested_level, full_name, document_type, document_ref)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(user_id)
    .bind(req.requested_level)
    .bind(req.full_name.trim())
    .bind(&req.document_type)
    .bind(req.document_ref.trim())
    .execute(state.ledger.pool())
    .await;

    if let Err(sqlx::Error::Database(ref e)) = result {
        if e.is_unique_violation() {
            return Err(ApiError::Conflict(
                "you already have a pending KYC submission".to_string(),
            ));
        }
    }
    result.map_err(StorageError::from)?;

    Ok((
        StatusCode::CREATED,
        Json(KycSubmissionResponse {
            id,
            status: "pending".to_string(),
            requested_level: req.requested_level,
        }),
    ))
}

pub async fn get_kyc_status(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
) -> ApiResult<Json<KycStatusResponse>> {
    let row = sqlx::query(
        "SELECT u.kyc_level, s.id, s.status, s.requested_level
         FROM users u
         LEFT JOIN LATERAL (
             SELECT id, status, requested_level FROM kyc_submissions
             WHERE user_id = u.id ORDER BY created_at DESC LIMIT 1
         ) s ON TRUE
         WHERE u.id = $1",
    )
    .bind(user_id)
    .fetch_optional(state.ledger.pool())
    .await
    .map_err(StorageError::from)?
    .ok_or_else(|| ApiError::Unauthorized("unknown user".to_string()))?;

    let kyc_level: i16 = row.try_get("kyc_level").map_err(StorageError::from)?;
    let sid: Option<Uuid> = row.try_get("id").map_err(StorageError::from)?;
    let latest_submission = match sid {
        Some(id) => Some(KycSubmissionResponse {
            id,
            status: row.try_get("status").map_err(StorageError::from)?,
            requested_level: row.try_get("requested_level").map_err(StorageError::from)?,
        }),
        None => None,
    };
    Ok(Json(KycStatusResponse {
        kyc_level,
        latest_submission,
    }))
}

// Reviewing your own submission is refused; anything else that did not update is not pending.
async fn unreviewable(
    tx: &mut sqlx::PgConnection,
    submission_id: Uuid,
    admin_id: Uuid,
) -> ApiResult<ApiError> {
    let own: Option<bool> = sqlx::query_scalar(
        "SELECT user_id = $2 FROM kyc_submissions WHERE id = $1 AND status = 'pending'",
    )
    .bind(submission_id)
    .bind(admin_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(StorageError::from)?;
    Ok(if own == Some(true) {
        ApiError::DualControlRequired("admins cannot review their own KYC submission".to_string())
    } else {
        ApiError::NotFound("no pending KYC submission with that id".to_string())
    })
}

pub async fn approve_kyc(
    AdminUser(admin_id): AdminUser,
    State(state): State<AppState>,
    Path(submission_id): Path<Uuid>,
) -> ApiResult<Json<KycSubmissionResponse>> {
    let mut tx = state
        .ledger
        .pool()
        .begin()
        .await
        .map_err(StorageError::from)?;

    let row = sqlx::query(
        "UPDATE kyc_submissions
         SET status = 'approved', reviewed_by = $2, reviewed_at = now()
         WHERE id = $1 AND status = 'pending' AND user_id <> $2
         RETURNING user_id, requested_level",
    )
    .bind(submission_id)
    .bind(admin_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(StorageError::from)?;

    let Some(row) = row else {
        return Err(unreviewable(&mut tx, submission_id, admin_id).await?);
    };
    let user_id: Uuid = row.try_get("user_id").map_err(StorageError::from)?;
    let requested_level: i16 = row.try_get("requested_level").map_err(StorageError::from)?;

    sqlx::query("UPDATE users SET kyc_level = GREATEST(kyc_level, $2) WHERE id = $1")
        .bind(user_id)
        .bind(requested_level)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?;
    audit(
        &mut tx,
        admin_id,
        "kyc.approve",
        Some(&submission_id.to_string()),
        serde_json::json!({ "user_id": user_id, "level": requested_level }),
    )
    .await?;

    tx.commit().await.map_err(StorageError::from)?;
    Ok(Json(KycSubmissionResponse {
        id: submission_id,
        status: "approved".to_string(),
        requested_level,
    }))
}

#[derive(Deserialize)]
pub struct RejectKycRequest {
    reason: String,
}

pub async fn reject_kyc(
    AdminUser(admin_id): AdminUser,
    State(state): State<AppState>,
    Path(submission_id): Path<Uuid>,
    Json(req): Json<RejectKycRequest>,
) -> ApiResult<Json<KycSubmissionResponse>> {
    let mut tx = state
        .ledger
        .pool()
        .begin()
        .await
        .map_err(StorageError::from)?;
    let row = sqlx::query(
        "UPDATE kyc_submissions
         SET status = 'rejected', reviewed_by = $2, reviewed_at = now(), rejection_reason = $3
         WHERE id = $1 AND status = 'pending' AND user_id <> $2
         RETURNING user_id, requested_level",
    )
    .bind(submission_id)
    .bind(admin_id)
    .bind(&req.reason)
    .fetch_optional(&mut *tx)
    .await
    .map_err(StorageError::from)?;

    let Some(row) = row else {
        return Err(unreviewable(&mut tx, submission_id, admin_id).await?);
    };
    let user_id: Uuid = row.try_get("user_id").map_err(StorageError::from)?;
    let requested_level: i16 = row.try_get("requested_level").map_err(StorageError::from)?;
    audit(
        &mut tx,
        admin_id,
        "kyc.reject",
        Some(&submission_id.to_string()),
        serde_json::json!({ "user_id": user_id, "reason": req.reason }),
    )
    .await?;
    tx.commit().await.map_err(StorageError::from)?;
    Ok(Json(KycSubmissionResponse {
        id: submission_id,
        status: "rejected".to_string(),
        requested_level,
    }))
}

pub const MAX_DOCUMENT_BYTES: usize = 5 * 1024 * 1024;

fn document_extension(content_type: &str) -> Option<&'static str> {
    match content_type {
        "image/jpeg" => Some("jpg"),
        "image/png" => Some("png"),
        "image/webp" => Some("webp"),
        "application/pdf" => Some("pdf"),
        _ => None,
    }
}

fn magic_matches(ext: &str, data: &[u8]) -> bool {
    match ext {
        "jpg" => data.starts_with(&[0xFF, 0xD8, 0xFF]),
        "png" => data.starts_with(b"\x89PNG\r\n\x1a\n"),
        "webp" => data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP",
        "pdf" => data.starts_with(b"%PDF-"),
        _ => false,
    }
}

#[derive(Serialize)]
pub struct DocumentResponse {
    document_ref: String,
}

pub async fn upload_kyc_document(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> ApiResult<(StatusCode, Json<DocumentResponse>)> {
    let uploaded_today: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM kyc_documents
         WHERE user_id = $1 AND created_at >= now() - interval '24 hours'",
    )
    .bind(user_id)
    .fetch_one(state.ledger.pool())
    .await
    .map_err(StorageError::from)?;
    if uploaded_today >= state.kyc_upload_daily_max {
        return Err(ApiError::TooManyRequests);
    }

    let field = loop {
        match multipart
            .next_field()
            .await
            .map_err(|e| ApiError::BadRequest(format!("malformed multipart body: {e}")))?
        {
            None => {
                return Err(ApiError::BadRequest(
                    "expected a multipart field named 'file'".to_string(),
                ))
            }
            Some(f) if f.name() == Some("file") => break f,
            Some(_) => continue,
        }
    };

    let ext = field
        .content_type()
        .and_then(document_extension)
        .ok_or_else(|| {
            ApiError::BadRequest(
                "file must be image/jpeg, image/png, image/webp or application/pdf".to_string(),
            )
        })?;
    let data = field
        .bytes()
        .await
        .map_err(|_| ApiError::BadRequest("document exceeds the 5 MB limit".to_string()))?;
    if data.is_empty() {
        return Err(ApiError::BadRequest("document is empty".to_string()));
    }
    if data.len() > MAX_DOCUMENT_BYTES {
        return Err(ApiError::BadRequest(
            "document exceeds the 5 MB limit".to_string(),
        ));
    }
    if !magic_matches(ext, &data) {
        return Err(ApiError::BadRequest(
            "file content does not match its declared type".to_string(),
        ));
    }

    let document_ref = format!("{}.{ext}", Uuid::new_v4());
    sqlx::query("INSERT INTO kyc_documents (document_ref, user_id, bytes) VALUES ($1, $2, $3)")
        .bind(&document_ref)
        .bind(user_id)
        .bind(data.len() as i64)
        .execute(state.ledger.pool())
        .await
        .map_err(StorageError::from)?;
    tokio::fs::create_dir_all(&state.document_dir)
        .await
        .map_err(|e| ApiError::Internal(format!("document store unavailable: {e}")))?;
    if let Err(e) = tokio::fs::write(state.document_dir.join(&document_ref), &data).await {
        let _ = sqlx::query("DELETE FROM kyc_documents WHERE document_ref = $1")
            .bind(&document_ref)
            .execute(state.ledger.pool())
            .await;
        return Err(ApiError::Internal(format!("failed to store document: {e}")));
    }
    tracing::info!(%user_id, %document_ref, bytes = data.len(), "kyc document uploaded");

    Ok((StatusCode::CREATED, Json(DocumentResponse { document_ref })))
}

pub async fn get_kyc_document(
    _admin: AdminUser,
    State(state): State<AppState>,
    Path(document_ref): Path<String>,
) -> ApiResult<Response> {
    let (stem, ext) = document_ref
        .rsplit_once('.')
        .ok_or_else(|| ApiError::BadRequest("malformed document ref".to_string()))?;
    if Uuid::parse_str(stem).is_err() {
        return Err(ApiError::BadRequest("malformed document ref".to_string()));
    }
    let content_type = match ext {
        "jpg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        _ => return Err(ApiError::BadRequest("malformed document ref".to_string())),
    };

    let bytes = tokio::fs::read(state.document_dir.join(&document_ref))
        .await
        .map_err(|_| ApiError::NotFound("document not found".to_string()))?;
    Ok((
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, content_type),
            (axum::http::header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (axum::http::header::CONTENT_SECURITY_POLICY, "sandbox"),
        ],
        bytes,
    )
        .into_response())
}
