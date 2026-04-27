//! Embedded SvelteKit UI, served at `/ui/*`. See
//! `docs/design/19-web-frontend.md`.
//!
//! `rust_embed` reads `frontend/build/` at compile time and bakes every file
//! into the binary. The handlers below resolve a request path against that
//! virtual filesystem, falling back to the SPA fallback (`200.html`) for
//! unknown paths under `/ui/*` so client-side routing in SvelteKit can take
//! over for dynamic routes like `/file/[...path]`.

use axum::{
    body::Body,
    extract::Path,
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Router,
};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "../../frontend/build"]
struct UiAssets;

const FALLBACK_HTML: &str = "200.html";

pub fn router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/", get(redirect_root))
        .route("/ui", get(serve_fallback))
        .route("/ui/", get(serve_fallback))
        .route("/ui/*path", get(serve_path))
}

async fn redirect_root() -> Redirect {
    Redirect::temporary("/ui/")
}

async fn serve_fallback() -> Response {
    serve_asset(FALLBACK_HTML, true)
}

async fn serve_path(Path(path): Path<String>) -> Response {
    // Try the asset first.
    if let Some(asset) = UiAssets::get(&path) {
        return asset_response(&path, asset);
    }
    // No asset matched. SvelteKit dynamic routes can have file-like
    // segments (`/ui/file/needle.txt`, `/ui/probes/build`), so we can't
    // 404 on every missing path with an extension. Only paths that look
    // like real asset references — `_app/...` (SvelteKit hashed bundles)
    // or `favicon.*` and similar top-level statics — should 404. Anything
    // else falls through to the SPA fallback so client-side routing wins.
    if is_asset_like(&path) {
        StatusCode::NOT_FOUND.into_response()
    } else {
        serve_asset(FALLBACK_HTML, true)
    }
}

fn is_asset_like(path: &str) -> bool {
    if path.starts_with("_app/") {
        return true;
    }
    // Top-level files SvelteKit might emit at the build root — favicon,
    // manifest.json, etc. These are bare names with an extension.
    let last = path.rsplit('/').next().unwrap_or("");
    !path.contains('/') && last.contains('.')
}

fn serve_asset(path: &str, html: bool) -> Response {
    match UiAssets::get(path) {
        Some(asset) => {
            let mut resp = asset_response(path, asset);
            if html {
                // SPA fallback should never be cached — it's the boot
                // document and we want users to pick up the latest build.
                resp.headers_mut()
                    .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            }
            resp
        }
        None => {
            // Should never happen if the build ran correctly. Fail loudly.
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("ui asset missing: {path}"),
            )
                .into_response()
        }
    }
}

fn asset_response(path: &str, asset: rust_embed::EmbeddedFile) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let body = Body::from(asset.data.into_owned());
    let mut resp = Response::new(body);
    if let Ok(v) = HeaderValue::from_str(mime.as_ref()) {
        resp.headers_mut().insert(header::CONTENT_TYPE, v);
    }
    // Hashed asset paths under `_app/immutable/` are content-addressable;
    // mark them long-lived. Everything else stays uncached (or short).
    if path.starts_with("_app/immutable/") {
        resp.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=31536000, immutable"),
        );
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The build directory must contain the SPA fallback. If it doesn't,
    /// the embedded UI is broken at runtime — fail at compile/test time
    /// instead of when a browser hits the gateway.
    #[test]
    fn fallback_is_embedded() {
        assert!(
            UiAssets::get(FALLBACK_HTML).is_some(),
            "frontend/build/{FALLBACK_HTML} must exist when building with `embed-ui`. \
             Run `npm run build` in frontend/, or build with `--no-default-features`."
        );
    }
}
