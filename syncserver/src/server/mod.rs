//! Main application server

use std::{sync::Arc, time::Duration};

use actix_cors::Cors;
use actix_web::{
    dev,
    http::StatusCode,
    http::{header::LOCATION, Method},
    middleware::errhandlers::ErrorHandlers,
    web, App, HttpRequest, HttpResponse, HttpServer,
};
use cadence::StatsdClient;
use syncserver_db_common::DbPool;
use syncserver_settings::Settings;
use syncstorage_settings::{Deadman, ServerLimits};
use tokio::sync::RwLock;

use crate::db::{pool_from_settings, spawn_pool_periodic_reporter};
use crate::error::ApiError;
use crate::server::metrics::Metrics;
use crate::tokenserver;
use crate::web::{handlers, middleware};

pub const BSO_ID_REGEX: &str = r"[ -~]{1,64}";
pub const COLLECTION_ID_REGEX: &str = r"[a-zA-Z0-9._-]{1,32}";
pub const SYNC_DOCS_URL: &str =
    "https://mozilla-services.readthedocs.io/en/latest/storage/apis-1.5.html";
const MYSQL_UID_REGEX: &str = r"[0-9]{1,10}";
const SYNC_VERSION_PATH: &str = "1.5";

pub mod metrics;
#[cfg(test)]
mod test;
pub mod user_agent;

/// This is the global HTTP state object that will be made available to all
/// HTTP API calls.
pub struct ServerState {
    pub db_pool: Box<dyn DbPool>,

    /// Server-enforced limits for request payloads.
    pub limits: Arc<ServerLimits>,

    /// limits rendered as JSON
    pub limits_json: String,

    /// Metric reporting
    pub metrics: Box<StatsdClient>,

    pub port: u16,

    pub quota_enabled: bool,

    pub deadman: Arc<RwLock<Deadman>>,
}

pub fn cfg_path(path: &str) -> String {
    let path = path
        .replace(
            "{collection}",
            &format!("{{collection:{}}}", COLLECTION_ID_REGEX),
        )
        .replace("{bso}", &format!("{{bso:{}}}", BSO_ID_REGEX));
    format!("/{}/{{uid:{}}}{}", SYNC_VERSION_PATH, MYSQL_UID_REGEX, path)
}

pub struct Server;

#[macro_export]
macro_rules! build_app {
    ($syncstorage_state: expr, $tokenserver_state: expr, $secrets: expr, $limits: expr, $cors: expr) => {
        App::new()
            .data($syncstorage_state)
            .data($tokenserver_state)
            .data($secrets)
            // Middleware is applied LIFO
            // These will wrap all outbound responses with matching status codes.
            .wrap(ErrorHandlers::new().handler(StatusCode::NOT_FOUND, ApiError::render_404))
            // These are our wrappers
            .wrap(middleware::weave::WeaveTimestamp::new())
            .wrap(tokenserver::logging::LoggingWrapper::new())
            .wrap(middleware::sentry::SentryWrapper::default())
            .wrap(middleware::rejectua::RejectUA::default())
            .wrap($cors)
            .wrap_fn(middleware::emit_http_status_with_tokenserver_origin)
            .service(
                web::resource(&cfg_path("/info/collections"))
                    .route(web::get().to(handlers::get_collections)),
            )
            .service(
                web::resource(&cfg_path("/info/collection_counts"))
                    .route(web::get().to(handlers::get_collection_counts)),
            )
            .service(
                web::resource(&cfg_path("/info/collection_usage"))
                    .route(web::get().to(handlers::get_collection_usage)),
            )
            .service(
                web::resource(&cfg_path("/info/configuration"))
                    .route(web::get().to(handlers::get_configuration)),
            )
            .service(
                web::resource(&cfg_path("/info/quota")).route(web::get().to(handlers::get_quota)),
            )
            .service(web::resource(&cfg_path("")).route(web::delete().to(handlers::delete_all)))
            .service(
                web::resource(&cfg_path("/storage")).route(web::delete().to(handlers::delete_all)),
            )
            .service(
                web::resource(&cfg_path("/storage/{collection}"))
                    .app_data(
                        // Declare the payload limit for "normal" collections.
                        web::PayloadConfig::new($limits.max_request_bytes as usize),
                    )
                    .app_data(
                        // Declare the payload limits for "JSON" payloads
                        // (Specify "text/plain" for legacy client reasons)
                        web::JsonConfig::default()
                            .limit($limits.max_request_bytes as usize)
                            .content_type(|ct| ct == mime::TEXT_PLAIN),
                    )
                    .route(web::delete().to(handlers::delete_collection))
                    .route(web::get().to(handlers::get_collection))
                    .route(web::post().to(handlers::post_collection)),
            )
            .service(
                web::resource(&cfg_path("/storage/{collection}/{bso}"))
                    .app_data(web::PayloadConfig::new($limits.max_request_bytes as usize))
                    .app_data(
                        web::JsonConfig::default()
                            .limit($limits.max_request_bytes as usize)
                            .content_type(|ct| ct == mime::TEXT_PLAIN),
                    )
                    .route(web::delete().to(handlers::delete_bso))
                    .route(web::get().to(handlers::get_bso))
                    .route(web::put().to(handlers::put_bso)),
            )
            // Tokenserver
            .service(
                web::resource("/1.0/{application}/{version}")
                    .route(web::get().to(tokenserver::handlers::get_tokenserver_result)),
            )
            // Dockerflow
            // Remember to update .::web::middleware::DOCKER_FLOW_ENDPOINTS
            // when applying changes to endpoint names.
            .service(web::resource("/__heartbeat__").route(web::get().to(handlers::heartbeat)))
            .service(web::resource("/__lbheartbeat__").route(web::get().to(
                handlers::lbheartbeat, /*
                                           |_: HttpRequest| {
                                           // used by the load balancers, just return OK.
                                           HttpResponse::Ok()
                                               .content_type("application/json")
                                               .body("{}")
                                       }
                                       */
            )))
            .service(
                web::resource("/__version__").route(web::get().to(|_: HttpRequest| {
                    // return the contents of the version.json file created by circleci
                    // and stored in the docker root
                    HttpResponse::Ok()
                        .content_type("application/json")
                        .body(include_str!("../../version.json"))
                })),
            )
            .service(web::resource("/__error__").route(web::get().to(handlers::test_error)))
            .service(web::resource("/").route(web::get().to(|_: HttpRequest| {
                HttpResponse::Found()
                    .header(LOCATION, SYNC_DOCS_URL)
                    .finish()
            })))
    };
}

#[macro_export]
macro_rules! build_app_without_syncstorage {
    ($state: expr, $secrets: expr, $cors: expr) => {
        App::new()
            .data($state)
            .data($secrets)
            // Middleware is applied LIFO
            // These will wrap all outbound responses with matching status codes.
            .wrap(ErrorHandlers::new().handler(StatusCode::NOT_FOUND, ApiError::render_404))
            // These are our wrappers
            .wrap(middleware::sentry::SentryWrapper::default())
            .wrap(tokenserver::logging::LoggingWrapper::new())
            .wrap(middleware::rejectua::RejectUA::default())
            // Followed by the "official middleware" so they run first.
            // actix is getting increasingly tighter about CORS headers. Our server is
            // not a huge risk but does deliver XHR JSON content.
            // For now, let's be permissive and use NGINX (the wrapping server)
            // for finer grained specification.
            .wrap($cors)
            .service(
                web::resource("/1.0/{application}/{version}")
                    .route(web::get().to(tokenserver::handlers::get_tokenserver_result)),
            )
            // Dockerflow
            // Remember to update .::web::middleware::DOCKER_FLOW_ENDPOINTS
            // when applying changes to endpoint names.
            .service(
                web::resource("/__heartbeat__")
                    .route(web::get().to(tokenserver::handlers::heartbeat)),
            )
            .service(
                web::resource("/__lbheartbeat__").route(web::get().to(|_: HttpRequest| {
                    // used by the load balancers, just return OK.
                    HttpResponse::Ok()
                        .content_type("application/json")
                        .body("{}")
                })),
            )
            .service(
                web::resource("/__version__").route(web::get().to(|_: HttpRequest| {
                    // return the contents of the version.json file created by circleci
                    // and stored in the docker root
                    HttpResponse::Ok()
                        .content_type("application/json")
                        .body(include_str!("../../version.json"))
                })),
            )
            .service(
                web::resource("/__error__").route(web::get().to(tokenserver::handlers::test_error)),
            )
            .service(web::resource("/").route(web::get().to(|_: HttpRequest| {
                HttpResponse::Found()
                    .header(LOCATION, SYNC_DOCS_URL)
                    .finish()
            })))
    };
}

impl Server {
    pub async fn with_settings(settings: Settings) -> Result<dev::Server, ApiError> {
        let settings_copy = settings.clone();
        let metrics = metrics::metrics_from_opts(
            &settings.syncstorage.statsd_label,
            settings.statsd_host.as_deref(),
            settings.statsd_port,
        )?;
        let host = settings.host.clone();
        let port = settings.port;
        let deadman = Arc::new(RwLock::new(Deadman::from(&settings.syncstorage)));
        let db_pool = pool_from_settings(&settings.syncstorage, &Metrics::from(&metrics)).await?;
        let limits = Arc::new(settings.syncstorage.limits);
        let limits_json =
            serde_json::to_string(&*limits).expect("ServerLimits failed to serialize");
        let secrets = Arc::new(settings.master_secret);
        let quota_enabled = settings.syncstorage.enable_quota;
        let actix_keep_alive = settings.actix_keep_alive;
        let tokenserver_state = if settings.tokenserver.enabled {
            let state = tokenserver::ServerState::from_settings(
                &settings.tokenserver,
                metrics::metrics_from_opts(
                    &settings.tokenserver.statsd_label,
                    settings.statsd_host.as_deref(),
                    settings.statsd_port,
                )?,
            )?;

            spawn_pool_periodic_reporter(
                Duration::from_secs(10),
                *state.metrics.clone(),
                state.db_pool.clone(),
            )?;

            Some(state)
        } else {
            None
        };

        spawn_pool_periodic_reporter(Duration::from_secs(10), metrics.clone(), db_pool.clone())?;

        let mut server = HttpServer::new(move || {
            let syncstorage_state = ServerState {
                db_pool: db_pool.clone(),
                limits: Arc::clone(&limits),
                limits_json: limits_json.clone(),
                metrics: Box::new(metrics.clone()),
                port,
                quota_enabled,
                deadman: Arc::clone(&deadman),
            };

            build_app!(
                syncstorage_state,
                tokenserver_state.clone(),
                Arc::clone(&secrets),
                limits,
                build_cors(&settings_copy)
            )
        });

        if let Some(keep_alive) = actix_keep_alive {
            server = server.keep_alive(keep_alive as usize);
        }

        let server = server
            .bind(format!("{}:{}", host, port))
            .expect("Could not get Server in Server::with_settings")
            .run();
        Ok(server)
    }

    pub async fn tokenserver_only_with_settings(
        settings: Settings,
    ) -> Result<dev::Server, ApiError> {
        let settings_copy = settings.clone();
        let host = settings.host.clone();
        let port = settings.port;
        let secrets = Arc::new(settings.master_secret.clone());
        let tokenserver_state = tokenserver::ServerState::from_settings(
            &settings.tokenserver,
            metrics::metrics_from_opts(
                &settings.tokenserver.statsd_label,
                settings.statsd_host.as_deref(),
                settings.statsd_port,
            )?,
        )?;

        spawn_pool_periodic_reporter(
            Duration::from_secs(10),
            *tokenserver_state.metrics.clone(),
            tokenserver_state.db_pool.clone(),
        )?;

        let server = HttpServer::new(move || {
            build_app_without_syncstorage!(
                Some(tokenserver_state.clone()),
                Arc::clone(&secrets),
                build_cors(&settings_copy)
            )
        });

        let server = server
            .bind(format!("{}:{}", host, port))
            .expect("Could not get Server in Server::with_settings")
            .run();
        Ok(server)
    }
}

pub fn build_cors(settings: &Settings) -> Cors {
    // Followed by the "official middleware" so they run first.
    // actix is getting increasingly tighter about CORS headers. Our server is
    // not a huge risk but does deliver XHR JSON content.
    // For now, let's be permissive and use NGINX (the wrapping server)
    // for finer grained specification.
    let mut cors = Cors::default();

    if let Some(allowed_origin) = &settings.cors_allowed_origin {
        cors = cors.allowed_origin(allowed_origin);
    }

    if let Some(allowed_methods) = &settings.cors_allowed_methods {
        let mut methods = vec![];
        for method_string in allowed_methods {
            let method = Method::from_bytes(method_string.as_bytes()).unwrap();
            methods.push(method);
        }
        cors = cors.allowed_methods(methods);
    }
    if let Some(allowed_headers) = &settings.cors_allowed_headers {
        cors = cors.allowed_headers(allowed_headers);
    }

    if let Some(max_age) = &settings.cors_max_age {
        cors = cors.max_age(*max_age);
    }

    cors
}
