use crate::{auth::AuthedBusiness, error::AppError, models::customer::*, AppState};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use uuid::Uuid;

pub async fn create_customer(
    State(state): State<AppState>,
    AuthedBusiness(business_id): AuthedBusiness,
    Json(req): Json<CreateCustomerRequest>,
) -> Result<(StatusCode, Json<Customer>), AppError> {
    if req.name.trim().is_empty() || req.email.trim().is_empty() {
        return Err(AppError::bad_request(
            "invalid_customer",
            "name and email are both required",
        ));
    }

    let customer = sqlx::query_as::<_, Customer>(
        "INSERT INTO customers (business_id, name, email) VALUES ($1, $2, $3)
         RETURNING id, business_id, name, email, created_at",
    )
    .bind(business_id)
    .bind(&req.name)
    .bind(&req.email)
    .fetch_one(&state.db)
    .await?;

    Ok((StatusCode::CREATED, Json(customer)))
}

pub async fn get_customer(
    State(state): State<AppState>,
    AuthedBusiness(business_id): AuthedBusiness,
    Path(customer_id): Path<Uuid>,
) -> Result<Json<Customer>, AppError> {
    let customer = sqlx::query_as::<_, Customer>(
        "SELECT id, business_id, name, email, created_at FROM customers
         WHERE id = $1 AND business_id = $2",
    )
    .bind(customer_id)
    .bind(business_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::not_found("customer_not_found", "no such customer"))?;

    Ok(Json(customer))
}

pub async fn list_customers(
    State(state): State<AppState>,
    AuthedBusiness(business_id): AuthedBusiness,
) -> Result<Json<Vec<Customer>>, AppError> {
    let customers = sqlx::query_as::<_, Customer>(
        "SELECT id, business_id, name, email, created_at FROM customers
         WHERE business_id = $1 ORDER BY created_at DESC",
    )
    .bind(business_id)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(customers))
}
