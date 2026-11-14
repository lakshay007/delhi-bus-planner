//! HTTP API. Documented in README.md; the web page in web/index.html is a client of it.

use crate::clock::{Local, now_epoch};
use crate::planner::{Engine, PlanError, plan};
use crate::rt;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Json};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

pub struct App {
    pub engine: Engine,
    pub raw_dir: PathBuf,
}

#[derive(Deserialize)]
struct JourneyQuery {
    from: Option<String>,
    to: Option<String>,
    depart: Option<String>,
    max: Option<usize>,
}

fn bad(msg: impl Into<String>) -> axum::response::Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": msg.into() }))).into_response()
}

async fn journeys(State(app): State<Arc<App>>, Query(q): Query<JourneyQuery>) -> axum::response::Response {
    let (Some(from), Some(to)) = (q.from, q.to) else {
        return bad("both `from` and `to` are required: a stop name or \"lat,lon\"");
    };
    let depart = match q.depart.as_deref().map(str::trim) {
        None | Some("") | Some("now") => Local::from_epoch(now_epoch()),
        Some(s) => match Local::parse(s) {
            Some(l) => l,
            None => return bad(format!("could not read depart=\"{s}\"; use YYYY-MM-DDTHH:MM in Delhi time, or omit for now")),
        },
    };
    let max = q.max.unwrap_or(3).clamp(1, 6);
    // The search is CPU-bound; keep it off the async threads.
    let res = tokio::task::spawn_blocking(move || plan(&app.engine, &from, &to, depart, max)).await;
    match res {
        Ok(Ok(r)) => Json(r).into_response(),
        Ok(Err(e @ PlanError::UnknownPlace(_))) => bad(e.to_string()),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

#[derive(Deserialize)]
struct StopQuery {
    q: Option<String>,
    limit: Option<usize>,
}

async fn stops(State(app): State<Arc<App>>, Query(q): Query<StopQuery>) -> Json<serde_json::Value> {
    let hits = app.engine.net.search_names(q.q.as_deref().unwrap_or(""), q.limit.unwrap_or(10).min(50));
    Json(json!(hits.into_iter().map(|(name, lat, lon, routes)| json!({"name": name, "lat": lat, "lon": lon, "routes": routes})).collect::<Vec<_>>()))
}

async fn health(State(app): State<Arc<App>>) -> Json<serde_json::Value> {
    let now = now_epoch();
    let today = Local::from_epoch(now).date_string();
    let last = rt::list_day(&app.raw_dir, &today).ok().and_then(|v| v.last().map(|x| x.0));
    Json(json!({
        "status": "ok",
        "network": app.engine.net.summary(),
        "travel_times": app.engine.times.name(),
        "basis": app.engine.basis,
        "live": app.engine.live.read().unwrap().as_ref().map(|l| json!({
            "as_of": Local::from_epoch(l.ratios.now).iso(),
            "traversals_last_hour": l.ratios.n_obs,
            "segments_with_live_data": l.ratios.n_segs,
            "city_ratio": l.ratios.city_ratio(),
            "buses_tracked": l.buses_tracked,
        })),
        "feed_archive": {
            "last_snapshot_age_s": last.map(|t| now - t),
            "collecting": last.is_some_and(|t| now - t < 120),
        },
    }))
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../web/index.html"))
}

pub async fn serve(app: App, port: u16) -> anyhow::Result<()> {
    let app = Arc::new(app);
    // Keep the live view current. Reading and linking the last hour of
    // snapshots takes well under a second, so a plain thread is enough.
    let bg = app.clone();
    std::thread::spawn(move || {
        let mut last_err = String::from("starting");
        loop {
            match bg.engine.refresh_live(&bg.raw_dir) {
                Ok(m) => {
                    if !last_err.is_empty() {
                        eprintln!("{m}");
                    }
                    last_err.clear();
                }
                Err(e) => {
                    let e = e.to_string();
                    if e != last_err {
                        eprintln!("live data not available: {e}");
                    }
                    last_err = e;
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(30));
        }
    });
    let router = Router::new()
        .route("/", get(index))
        .route("/journeys", get(journeys))
        .route("/stops", get(stops))
        .route("/health", get(health))
        .with_state(app);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    eprintln!("listening on http://127.0.0.1:{port}");
    axum::serve(listener, router).await?;
    Ok(())
}
