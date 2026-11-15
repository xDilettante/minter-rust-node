mod subscribe;

use crate::{
    google_rpc, json, parse_json,
    pb::{
        AddressRequest, BlockField, BlockRequest, CoinIdRequest, CoinInfoRequest,
        CommissionVotesRequest, EventsRequest, HaltsRequest, PriceCommissionRequest,
        SendTransactionRequest, TransactionRequest, TransactionsRequest, UpdateVotesRequest,
        VersionNetworkRequest, api_service_server::ApiService,
    },
};
use axum::{
    Json, Router,
    body::to_bytes,
    extract::{Path, Request as HttpRequest, State},
    http::{HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use prost::{Message, Name};
use std::{collections::BTreeMap, num::IntErrorKind, time::Duration};
use tonic::{Code, Request, Status};

pub fn router<S: ApiService + Clone>(service: S) -> Router {
    Router::new()
        .route(
            "/v2/subscribe",
            get(subscribe::subscribe::<S>)
                .post(subscribe::subscribe::<S>)
                .head(method_not_allowed),
        )
        .route(
            "/v2/status",
            get(status::<S>).post(status::<S>).head(method_not_allowed),
        )
        .route(
            "/v2/version_network",
            get(version_network::<S>).head(method_not_allowed),
        )
        .route(
            "/v2/price_commissions",
            get(price_commission::<S>).head(method_not_allowed),
        )
        .route(
            "/v2/commission_votes/{target_version}",
            get(commission_votes::<S>).head(method_not_allowed),
        )
        .route(
            "/v2/update_votes/{target_version}",
            get(update_votes::<S>).head(method_not_allowed),
        )
        .route(
            "/v2/halts/{height}",
            get(halts::<S>).head(method_not_allowed),
        )
        .route(
            "/v2/address/{address}",
            get(address::<S>)
                .post(address::<S>)
                .head(method_not_allowed),
        )
        .route(
            "/v2/coin_info/{symbol}",
            get(coin_info::<S>)
                .post(coin_info::<S>)
                .head(method_not_allowed),
        )
        .route(
            "/v2/coin_info_by_id/{id}",
            get(coin_info_by_id::<S>)
                .post(coin_info_by_id::<S>)
                .head(method_not_allowed),
        )
        .route(
            "/v2/block/{height}",
            get(block::<S>).post(block::<S>).head(method_not_allowed),
        )
        .route(
            "/v2/events/{height}",
            get(events::<S>).post(events::<S>).head(method_not_allowed),
        )
        .route(
            "/v2/transaction/{hash}",
            get(transaction::<S>)
                .post(transaction::<S>)
                .head(method_not_allowed),
        )
        .route(
            "/v2/transactions",
            get(transactions::<S>)
                .post(transactions::<S>)
                .head(method_not_allowed),
        )
        .route(
            "/v2/send_transaction/{tx}",
            get(send_path::<S>)
                .post(send_path::<S>)
                .head(method_not_allowed),
        )
        .route("/v2/send_transaction", post(send_body::<S>))
        .fallback(|| async { error_response(Status::not_found("Not Found")) })
        .method_not_allowed_fallback(method_not_allowed)
        .layer(middleware::from_fn(cors))
        .with_state(service)
}

async fn method_not_allowed() -> Response {
    error_response(Status::unimplemented("Method Not Allowed"))
}

async fn cors(request: HttpRequest, next: Next) -> Response {
    let Some(origin) = request
        .headers()
        .get(header::ORIGIN)
        .filter(|value| !value.is_empty())
        .cloned()
    else {
        return next.run(request).await;
    };
    let mut response = if request.method() == Method::OPTIONS
        && request
            .headers()
            .get(header::ACCESS_CONTROL_REQUEST_METHOD)
            .is_some_and(|value| !value.is_empty())
    {
        let mut response = StatusCode::OK.into_response();
        response.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("Content-Type,Accept"),
        );
        response.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET,HEAD,POST,PUT,DELETE"),
        );
        response
    } else {
        next.run(request).await
    };
    response
        .headers_mut()
        .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    response
}

async fn status<S: ApiService>(State(service): State<S>, request: HttpRequest) -> Response {
    if request.method() == Method::POST
        && let Err(error) = parameters(request).await
    {
        return error_response(error);
    }
    respond(service.status(Request::new(())).await)
}

async fn version_network<S: ApiService>(State(service): State<S>) -> Response {
    respond(
        service
            .version_network(Request::new(VersionNetworkRequest {}))
            .await,
    )
}

async fn price_commission<S: ApiService>(
    State(service): State<S>,
    request: HttpRequest,
) -> Response {
    match parameters(request)
        .await
        .and_then(|query| query_field(&query, "height", uint64))
    {
        Ok(height) => respond(
            service
                .price_commission(Request::new(PriceCommissionRequest { height }))
                .await,
        ),
        Err(error) => error_response(error),
    }
}

async fn commission_votes<S: ApiService>(
    State(service): State<S>,
    Path(target): Path<String>,
    request: HttpRequest,
) -> Response {
    match vote_parameters(&target, request).await {
        Ok((target_version, height)) => respond(
            service
                .commission_votes(Request::new(CommissionVotesRequest {
                    target_version,
                    height,
                }))
                .await,
        ),
        Err(error) => error_response(error),
    }
}

async fn update_votes<S: ApiService>(
    State(service): State<S>,
    Path(target): Path<String>,
    request: HttpRequest,
) -> Response {
    match vote_parameters(&target, request).await {
        Ok((target_version, height)) => respond(
            service
                .update_votes(Request::new(UpdateVotesRequest {
                    target_version,
                    height,
                }))
                .await,
        ),
        Err(error) => error_response(error),
    }
}

async fn vote_parameters(target: &str, request: HttpRequest) -> Result<(u64, u64), Status> {
    let target_version = path_number("target_version", target)?;
    let height = query_field(&parameters(request).await?, "height", uint64)?;
    Ok((target_version, height))
}

async fn halts<S: ApiService>(State(service): State<S>, Path(height): Path<String>) -> Response {
    match path_number("height", &height) {
        Ok(height) => respond(service.halts(Request::new(HaltsRequest { height })).await),
        Err(error) => error_response(error),
    }
}

fn path_number(name: &str, text: &str) -> Result<u64, Status> {
    uint64(text).map_err(|error| {
        Status::invalid_argument(format!("type mismatch, parameter: {name}, error: {error}"))
    })
}

async fn address<S: ApiService>(
    State(service): State<S>,
    Path(address): Path<String>,
    request: HttpRequest,
) -> Response {
    let req = parameters(request).await.and_then(|query| {
        Ok(AddressRequest {
            address,
            height: query_field(&query, "height", uint64)?,
            delegated: query_field(&query, "delegated", boolean)?,
        })
    });
    match req {
        Ok(req) => respond(service.address(Request::new(req)).await),
        Err(err) => error_response(err),
    }
}

async fn coin_info<S: ApiService>(
    State(service): State<S>,
    Path(symbol): Path<String>,
    request: HttpRequest,
) -> Response {
    match parameters(request)
        .await
        .and_then(|query| query_field(&query, "height", uint64))
    {
        Ok(height) => respond(
            service
                .coin_info(Request::new(CoinInfoRequest { height, symbol }))
                .await,
        ),
        Err(err) => error_response(err),
    }
}

async fn coin_info_by_id<S: ApiService>(
    State(service): State<S>,
    Path(id): Path<String>,
    request: HttpRequest,
) -> Response {
    let req = async {
        let id = uint64(&id).map_err(|err| {
            Status::invalid_argument(format!("type mismatch, parameter: id, error: {err}"))
        })?;
        Ok(CoinIdRequest {
            id,
            height: query_field(&parameters(request).await?, "height", uint64)?,
        })
    }
    .await;
    match req {
        Ok(req) => respond(service.coin_info_by_id(Request::new(req)).await),
        Err(err) => error_response(err),
    }
}

async fn block<S: ApiService>(
    State(service): State<S>,
    Path(height): Path<String>,
    request: HttpRequest,
) -> Response {
    let req = async {
        let height = uint64(&height).map_err(|err| {
            Status::invalid_argument(format!("type mismatch, parameter: height, error: {err}"))
        })?;
        let query = parameters(request).await?;
        let fields = query_values(&query, "fields")?
            .into_iter()
            .map(|value| {
                BlockField::from_str_name(&value)
                    .or_else(|| {
                        value
                            .parse::<i32>()
                            .ok()
                            .and_then(|n| BlockField::try_from(n).ok())
                    })
                    .map(|field| field as i32)
                    .ok_or_else(|| {
                        Status::invalid_argument(format!(
                            "parsing list \"fields\": {value:?} is not a valid value"
                        ))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(BlockRequest {
            height,
            fields,
            failed_txs: query_field(&query, "failed_txs", boolean)?,
            events: query_field(&query, "events", boolean)?,
        })
    }
    .await;
    match req {
        Ok(req) => respond(service.block(Request::new(req)).await),
        Err(err) => error_response(err),
    }
}

async fn transaction<S: ApiService>(
    State(service): State<S>,
    Path(hash): Path<String>,
    request: HttpRequest,
) -> Response {
    if request.method() == Method::POST
        && let Err(err) = parameters(request).await
    {
        return error_response(err);
    }
    respond(
        service
            .transaction(Request::new(TransactionRequest { hash }))
            .await,
    )
}

async fn events<S: ApiService>(
    State(service): State<S>,
    Path(height): Path<String>,
    request: HttpRequest,
) -> Response {
    let req = async {
        let height = uint64(&height).map_err(|err| {
            Status::invalid_argument(format!("type mismatch, parameter: height, error: {err}"))
        })?;
        let query = parameters(request).await?;
        Ok(EventsRequest {
            height,
            search: query_values(&query, "search")?,
        })
    }
    .await;
    match req {
        Ok(req) => respond(service.events(Request::new(req)).await),
        Err(err) => error_response(err),
    }
}

async fn transactions<S: ApiService>(State(service): State<S>, request: HttpRequest) -> Response {
    let req = parameters(request).await.and_then(|query| {
        Ok(TransactionsRequest {
            query: query_field(&query, "query", |value| Ok(value.to_owned()))?,
            page: query_field(&query, "page", int32)?,
            per_page: query_field(&query, "per_page", int32)?,
        })
    });
    match req {
        Ok(req) => respond(service.transactions(Request::new(req)).await),
        Err(err) => error_response(err),
    }
}

async fn send_path<S: ApiService>(
    State(service): State<S>,
    Path(tx): Path<String>,
    request: HttpRequest,
) -> Response {
    if request.method() == Method::POST
        && let Err(err) = parameters(request).await
    {
        return error_response(err);
    }
    respond(
        service
            .send_transaction(Request::new(SendTransactionRequest { tx }))
            .await,
    )
}

async fn send_body<S: ApiService>(State(service): State<S>, request: HttpRequest) -> Response {
    let req = body_bytes(request.into_body())
        .await
        .and_then(|bytes| parse_json(&bytes));
    match req {
        Ok(req) => respond(service.send_transaction(Request::new(req)).await),
        Err(err) => error_response(err),
    }
}

async fn body_bytes(body: axum::body::Body) -> Result<axum::body::Bytes, Status> {
    tokio::time::timeout(Duration::from_secs(10), to_bytes(body, 10 * 1024 * 1024))
        .await
        .map_err(|_| Status::deadline_exceeded("request body timed out"))?
        .map_err(|_| Status::invalid_argument("http: POST too large"))
}

async fn parameters(request: HttpRequest) -> Result<String, Status> {
    let (parts, body) = request.into_parts();
    let query = parts.uri.query().unwrap_or_default();
    if parts.method == Method::GET {
        validate_query(query)?;
        return Ok(query.to_owned());
    }
    if parts
        .headers
        .get(header::CONTENT_TYPE)
        .map(HeaderValue::as_bytes)
        != Some(b"application/x-www-form-urlencoded")
    {
        return Err(Status::unimplemented("Method Not Allowed"));
    }
    if let Some(method) = parts
        .headers
        .get("x-http-method-override")
        .filter(|method| !method.is_empty())
    {
        if method.as_bytes().eq_ignore_ascii_case(b"GET") {
            validate_query(query)?;
            return Ok(query.to_owned());
        }
        if !method.as_bytes().eq_ignore_ascii_case(b"POST") {
            return Err(Status::unimplemented("Method Not Allowed"));
        }
    }
    let form = body_bytes(body).await?;
    let form = std::str::from_utf8(&form)
        .map_err(|_| Status::invalid_argument("invalid form encoding"))?;
    validate_query(form)?;
    validate_query(query)?;
    Ok(format!("{form}&{query}"))
}

fn validate_query(query: &str) -> Result<(), Status> {
    for pair in query.split('&') {
        if pair.contains(';') {
            return Err(Status::invalid_argument(
                "invalid semicolon separator in query",
            ));
        }
        let mut bytes = pair.as_bytes();
        while let Some(index) = bytes.iter().position(|&byte| byte == b'%') {
            bytes = &bytes[index..];
            if !matches!(bytes, [b'%', a, b, ..] if a.is_ascii_hexdigit() && b.is_ascii_hexdigit())
            {
                let escape = String::from_utf8_lossy(&bytes[..bytes.len().min(3)]);
                return Err(Status::invalid_argument(format!(
                    "invalid URL escape {escape:?}"
                )));
            }
            bytes = &bytes[3..];
        }
    }
    Ok(())
}

fn query_field<T: Default>(
    query: &str,
    name: &str,
    parse: impl FnOnce(&str) -> Result<T, String>,
) -> Result<T, Status> {
    match query_values(query, name)?.as_slice() {
        [] => Ok(T::default()),
        [value] => parse(value)
            .map_err(|err| Status::invalid_argument(format!("parsing field {name:?}: {err}"))),
        values => Err(Status::invalid_argument(format!(
            "too many values for field {name:?}: {}",
            values.join(", ")
        ))),
    }
}

fn query_values(query: &str, name: &str) -> Result<Vec<String>, Status> {
    let alias = match name {
        "failed_txs" => "failedTxs",
        "per_page" => "perPage",
        _ => name,
    };
    let mut values = Vec::new();
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        let (path, index) = key
            .strip_suffix(']')
            .and_then(|key| key.rsplit_once('['))
            .map_or((key.as_ref(), None), |(path, index)| (path, Some(index)));
        if let Some((first, _)) = path.split_once('.')
            && (first == name || first == alias)
        {
            return Err(Status::invalid_argument(format!(
                "invalid path: {first:?} is not a message"
            )));
        }
        if path == name || path == alias {
            if let Some(index) = index {
                values.push(index.to_owned());
            }
            values.push(value.into_owned());
        }
    }
    Ok(values)
}

fn boolean(value: &str) -> Result<bool, String> {
    match value {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Ok(true),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Ok(false),
        _ => Err(parse_error("ParseBool", value, "invalid syntax")),
    }
}

fn int32(value: &str) -> Result<i32, String> {
    value.parse::<i32>().map_err(|err| {
        let cause = match err.kind() {
            IntErrorKind::PosOverflow | IntErrorKind::NegOverflow => "value out of range",
            _ => "invalid syntax",
        };
        parse_error("ParseInt", value, cause)
    })
}

fn uint64(value: &str) -> Result<u64, String> {
    if value.is_empty() {
        return Err(parse_error("ParseUint", value, "invalid syntax"));
    }
    let mut parsed = 0u64;
    for byte in value.bytes() {
        if !byte.is_ascii_digit() {
            return Err(parse_error("ParseUint", value, "invalid syntax"));
        }
        parsed = parsed
            .checked_mul(10)
            .and_then(|n| n.checked_add(u64::from(byte - b'0')))
            .ok_or_else(|| parse_error("ParseUint", value, "value out of range"))?;
    }
    Ok(parsed)
}

fn parse_error(function: &str, value: &str, cause: &str) -> String {
    format!("strconv.{function}: parsing {value:?}: {cause}")
}

fn respond<T: Message + Name>(response: Result<tonic::Response<T>, Status>) -> Response {
    match response.and_then(|result| json(&result.into_inner())) {
        Ok(body) => Json(body).into_response(),
        Err(status) => error_response(status),
    }
}

fn error_response(error: Status) -> Response {
    let (status, body) = error_body(&error);
    (status, Json(body)).into_response()
}

fn http_status(code: Code) -> StatusCode {
    match code {
        Code::Ok => StatusCode::OK,
        Code::Cancelled => StatusCode::from_u16(499).expect("valid HTTP status"),
        Code::InvalidArgument | Code::FailedPrecondition | Code::OutOfRange => {
            StatusCode::BAD_REQUEST
        }
        Code::DeadlineExceeded => StatusCode::GATEWAY_TIMEOUT,
        Code::NotFound => StatusCode::NOT_FOUND,
        Code::AlreadyExists | Code::Aborted => StatusCode::CONFLICT,
        Code::PermissionDenied => StatusCode::FORBIDDEN,
        Code::Unauthenticated => StatusCode::UNAUTHORIZED,
        Code::ResourceExhausted => StatusCode::TOO_MANY_REQUESTS,
        Code::Unimplemented => StatusCode::NOT_IMPLEMENTED,
        Code::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        Code::Unknown | Code::Internal | Code::DataLoss => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn error_body(error: &Status) -> (StatusCode, serde_json::Value) {
    let status = http_status(error.code());
    let mut fields = grpc_details(error.details());
    let code = fields
        .remove("code")
        .unwrap_or_else(|| status.as_u16().to_string());
    (
        status,
        serde_json::json!({
            "error": { "code": code, "message": error.message(), "data": fields }
        }),
    )
}

fn grpc_details(bytes: &[u8]) -> BTreeMap<String, String> {
    let Some(detail) = google_rpc::Status::decode(bytes)
        .ok()
        .and_then(|status| status.details.into_iter().next())
        .filter(|detail| detail.type_url == "type.googleapis.com/google.protobuf.Struct")
        .and_then(|detail| prost_types::Struct::decode(detail.value.as_slice()).ok())
    else {
        return BTreeMap::new();
    };
    detail
        .fields
        .into_iter()
        .filter_map(|(key, value)| {
            if let Some(prost_types::value::Kind::StringValue(value)) = value.kind {
                Some((key, value))
            } else {
                None
            }
        })
        .collect()
}
