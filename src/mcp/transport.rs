use super::*;

pub enum Transport {
    Stdio,
    Http { host: String, port: u16 },
}

pub fn serve(
    paths: Paths,
    cfg: Config,
    spec: model::Spec,
    transport: Transport,
) -> crate::error::Result<()> {
    tracing::info!(
        version = crate::VERSION,
        db = %paths.db().display(),
        model = %cfg.model,
        dim = cfg.dim,
        device = %cfg.device,
        precision = %cfg.precision,
        offline = spec.offline,
        "starting ellmephant mcp"
    );
    let watch = Arc::new(crate::http::LockWatch::new());
    let state = crate::http::HttpState::new(
        paths.clone(),
        cfg.clone(),
        Arc::clone(&watch),
        std::time::Instant::now(),
    );
    let encode_interval = std::time::Duration::from_millis(cfg.encode_interval_ms);
    let server = MemoryServer::with_watch(
        Arc::new(Mutex::new(Session::open(paths, cfg, spec)?)),
        watch,
    );
    #[cfg(feature = "encoder")]
    let _worker = server.spawn_encode_worker(encode_interval);
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        match transport {
            Transport::Stdio => {
                let running = server
                    .serve(rmcp::transport::stdio())
                    .await
                    .map_err(|e| Error::Config(format!("mcp initialize failed: {e}")))?;
                running
                    .waiting()
                    .await
                    .map_err(|e| Error::Config(format!("mcp server task failed: {e}")))?;
                Ok(())
            }
            Transport::Http { host, port } => serve_http(server, &host, port, state).await,
        }
    })
}

pub fn http_router(
    server: MemoryServer,
    bind_host: &str,
    state: crate::http::HttpState,
) -> axum::Router {
    let config = if is_loopback(bind_host) {
        StreamableHttpServerConfig::default()
    } else {
        tracing::warn!(
            host = bind_host,
            "mcp http bound off-loopback; host-header checks off"
        );
        StreamableHttpServerConfig::default().disable_allowed_hosts()
    };
    const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
    let mut full_sessions = LocalSessionManager::default();
    full_sessions.session_config.keep_alive = Some(SESSION_IDLE_TIMEOUT);
    let shared = server.clone();
    let state_server = server.clone();
    let service: StreamableHttpService<MemoryServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(server.clone()),
            full_sessions.into(),
            config.clone(),
        );
    let mut core_sessions = LocalSessionManager::default();
    core_sessions.session_config.keep_alive = Some(SESSION_IDLE_TIMEOUT);
    let core_service: StreamableHttpService<MemoryServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(shared.clone().with_surface(Surface::Core)),
            core_sessions.into(),
            config,
        );
    let state = state.with_server(state_server);
    use crate::http_surface as surface;
    axum::Router::new()
        .route("/health", axum::routing::get(crate::http::health))
        .route("/whoami", axum::routing::get(surface::whoami))
        .route("/whois/{pen}", axum::routing::get(surface::whois))
        .route("/pen/mint", axum::routing::post(surface::pen_mint))
        .route("/pen/rotate", axum::routing::post(surface::pen_rotate))
        .route("/pen/lend", axum::routing::post(surface::pen_lend))
        .route("/sudo", axum::routing::post(surface::sudo))
        .route("/sudo/pass", axum::routing::post(surface::sudo_pass))
        .route("/sudo/release", axum::routing::post(surface::sudo_release))
        .route("/sudo/sessions", axum::routing::get(surface::sudo_sessions))
        .route("/bootstrap", axum::routing::get(surface::bootstrap))
        .route("/stats", axum::routing::get(surface::stats))
        .route("/memory-health", axum::routing::get(surface::memory_health))
        .route("/dangling", axum::routing::get(surface::dangling))
        .route(
            "/history",
            axum::routing::get(surface::history).delete(surface::history_erase),
        )
        .route(
            "/history/search",
            axum::routing::get(surface::history_search),
        )
        .route(
            "/memory/{key}",
            axum::routing::get(surface::memory_get).delete(surface::memory_delete),
        )
        .route(
            "/memory/{key}/backlinks",
            axum::routing::get(surface::backlinks),
        )
        .route("/memory/{key}/upvote", axum::routing::post(surface::upvote))
        .route("/memory/{key}/merge", axum::routing::post(surface::merge))
        .route("/memory/{key}/append", axum::routing::post(surface::append))
        .route("/memory/{key}/fold", axum::routing::post(surface::fold))
        .route("/memory/{key}/adopt", axum::routing::post(surface::adopt))
        .route("/memory", axum::routing::post(surface::upsert))
        .route("/search", axum::routing::get(surface::search))
        .route("/dupes", axum::routing::get(surface::dupes))
        .route("/projects", axum::routing::get(surface::projects))
        .route("/project/{key}", axum::routing::get(surface::project_get))
        .route("/project", axum::routing::post(surface::project_upsert))
        .route(
            "/config",
            axum::routing::get(surface::config_list).post(surface::config_define),
        )
        .route(
            "/config/{key}",
            axum::routing::get(surface::config_get).post(surface::config_set),
        )
        .route(
            "/claim",
            axum::routing::get(surface::claims).post(surface::claim_acquire),
        )
        .route(
            "/claim/{key}/release",
            axum::routing::post(surface::claim_release),
        )
        .route(
            "/mail",
            axum::routing::get(surface::mail_inbox).post(surface::mail_send),
        )
        .route("/mail/prune", axum::routing::post(surface::mail_prune))
        .route(
            "/channels",
            axum::routing::get(surface::channel_list).post(surface::channel_create),
        )
        .route(
            "/channels/breach",
            axum::routing::get(surface::channel_breach),
        )
        .route("/channels/wait", axum::routing::get(surface::channel_wait))
        .route(
            "/channels/{channel}/members",
            axum::routing::get(surface::channel_members),
        )
        .route(
            "/channels/{channel}/invite",
            axum::routing::post(surface::channel_invite),
        )
        .route(
            "/channels/{channel}/gate",
            axum::routing::post(surface::channel_set_gate),
        )
        .route(
            "/channels/{channel}/events",
            axum::routing::get(surface::channel_events),
        )
        .route(
            "/channels/{channel}/retention",
            axum::routing::post(surface::channel_set_retention),
        )
        .route(
            "/channels/prune",
            axum::routing::post(surface::channel_prune),
        )
        .route(
            "/channels/fetch",
            axum::routing::post(surface::channel_fetch),
        )
        .route(
            "/channels/present",
            axum::routing::post(surface::channel_present),
        )
        .route(
            "/channels/{channel}/messages",
            axum::routing::get(surface::channel_history).post(surface::channel_send),
        )
        .route(
            "/channels/{channel}/join",
            axum::routing::post(surface::channel_join),
        )
        .route(
            "/channels/{channel}/leave",
            axum::routing::post(surface::channel_leave),
        )
        .route(
            "/mail/surrogate/grant",
            axum::routing::post(surface::mail_surrogate_grant),
        )
        .route(
            "/mail/surrogate/revoke",
            axum::routing::post(surface::mail_surrogate_revoke),
        )
        .route(
            "/mail/surrogate",
            axum::routing::get(surface::mail_surrogate_list),
        )
        .route("/mail/ack", axum::routing::post(surface::mail_ack))
        .route("/mail/{seq}", axum::routing::get(surface::mail_read))
        .route("/reembed", axum::routing::post(surface::reembed))
        .route("/reparse", axum::routing::post(surface::reparse))
        .fallback(surface::not_found)
        .method_not_allowed_fallback(surface::method_not_allowed)
        .nest_service("/mcp", service)
        .nest_service("/mcp/core", core_service)
        .route("/api", axum::routing::get(surface::api_index))
        .route("/api/{group}", axum::routing::get(surface::api_group))
        .with_state(state)
}

fn is_loopback(host: &str) -> bool {
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

async fn serve_http(
    server: MemoryServer,
    host: &str,
    port: u16,
    state: crate::http::HttpState,
) -> crate::error::Result<()> {
    spawn_snapshotter(state.paths.clone());
    let listener = tokio::net::TcpListener::bind((host, port)).await?;
    let addr = listener.local_addr()?;
    tracing::info!(%addr, "ellmephant mcp listening on http://{addr}/mcp");
    let app = http_router(server, host, state);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

fn spawn_snapshotter(paths: crate::config::Paths) {
    let Some(every) = crate::store::snapshot::every() else {
        tracing::info!("snapshotter disabled (ELLMEPHANT_SNAPSHOT_HOURS=0)");
        return;
    };
    tokio::spawn(async move {
        loop {
            let root = paths.clone();
            let done =
                tokio::task::spawn_blocking(move || -> crate::error::Result<std::path::PathBuf> {
                    let conn = crate::db::open_read_only(&root)?;
                    crate::store::snapshot::write(&conn, root.root())
                })
                .await;
            match done {
                Ok(Ok(path)) => {
                    tracing::info!(path = %path.display(), "consistent snapshot written")
                }
                Ok(Err(err)) => tracing::warn!(error = %err, "snapshot failed"),
                Err(err) => tracing::warn!(error = %err, "snapshot task failed"),
            }
            tokio::time::sleep(every).await;
        }
    });
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("mcp shutting down");
}
