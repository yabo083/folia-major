//! Netease route table.
//!
//! Each row maps a frontend endpoint (see `src/services/netease.ts`) to the
//! upstream Netease API path and the transport protocol. Paths and request
//! bodies follow the reference package module files under
//! `folia-major/node_modules/@neteasecloudmusicapienhanced/api/module/`.
//! Per the M3 spec all content routes use weapi; the only xeapi route
//! (`/register/anonimous`) is rejected with HTTP 501.

/// Transport used to talk to music.163.com.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Protocol {
    Weapi,
    /// eapi forwarding is implemented (see `netease::eapi_forward` and the
    /// deterministic crypto vectors) but the M3 contract routes all use
    /// weapi; kept as a supported transport for routes that need it.
    #[allow(dead_code)]
    Eapi,
    /// XEAPI-only routes are not implemented; the server answers 501.
    Xeapi,
}

/// Special handling a route needs beyond "encrypt + forward".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RouteKind {
    /// Encrypt and forward, pass the upstream JSON through untouched.
    Plain,
    /// `/playlist/track/all`: fetch playlist track ids, slice, then song detail.
    TrackAll,
    /// `/login/qr/create`: build the login URL and render a QR image locally.
    QrCreate,
    /// `/login/qr/key`: wrap the upstream body under `data` with top-level `code`.
    QrKey,
    /// `/login/qr/check`: merge upstream Set-Cookie into the body as `cookie`.
    QrCheck,
    /// `/login/status`: wrap the upstream body under `data`.
    LoginStatus,
}

pub struct Route {
    pub name: &'static str,
    /// Upstream `/api/...` path; `{id}` is substituted from the `id` param.
    pub upstream: &'static str,
    pub protocol: Protocol,
    /// Whether the `id` query param is spliced into the upstream path.
    pub id_in_path: bool,
    pub kind: RouteKind,
}

pub const ROUTES: &[Route] = &[
    // Anonymous registration is XEAPI-only -> 501 (no fake success).
    Route {
        name: "/register/anonimous",
        upstream: "/api/register/anonimous",
        protocol: Protocol::Xeapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    // --- Login ---
    Route {
        name: "/login/qr/key",
        upstream: "/api/login/qrcode/unikey",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::QrKey,
    },
    Route {
        name: "/login/qr/create",
        upstream: "",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::QrCreate,
    },
    Route {
        name: "/login/qr/check",
        upstream: "/api/login/qrcode/client/login",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::QrCheck,
    },
    Route {
        name: "/login/status",
        upstream: "/api/w/nuser/account/get",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::LoginStatus,
    },
    Route {
        name: "/logout",
        upstream: "/api/logout",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    Route {
        name: "/user/account",
        upstream: "/api/nuser/account/get",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    // --- User data ---
    Route {
        name: "/like",
        upstream: "/api/radio/like",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    Route {
        name: "/likelist",
        upstream: "/api/song/like/get",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    Route {
        name: "/user/playlist",
        upstream: "/api/user/playlist",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    Route {
        name: "/user/cloud",
        upstream: "/api/v1/cloud/get",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    Route {
        name: "/user/cloud/detail",
        upstream: "/api/v1/cloud/get/byids",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    Route {
        name: "/cloud/lyric/get",
        upstream: "/api/cloud/lyric/get",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    // --- Playlist ---
    Route {
        name: "/playlist/detail",
        upstream: "/api/v6/playlist/detail",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    Route {
        name: "/playlist/track/all",
        upstream: "/api/v6/playlist/detail",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::TrackAll,
    },
    Route {
        name: "/playlist/tracks",
        upstream: "/api/playlist/manipulate/tracks",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    Route {
        name: "/playlist/subscribe",
        upstream: "/api/playlist/subscribe",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    Route {
        name: "/playlist/detail/dynamic",
        upstream: "/api/playlist/detail/dynamic",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    // --- Album ---
    Route {
        name: "/album",
        upstream: "/api/v1/album/{id}",
        protocol: Protocol::Weapi,
        id_in_path: true,
        kind: RouteKind::Plain,
    },
    Route {
        name: "/album/sublist",
        upstream: "/api/album/sublist",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    Route {
        name: "/album/sub",
        upstream: "/api/album/sub",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    Route {
        name: "/album/detail/dynamic",
        upstream: "/api/album/detail/dynamic",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    // --- Artist ---
    // The legacy `/api/artist/detail` weapi endpoint rejects requests with
    // "参数错误"; the reference client uses the eapi `/api/artist/head/info/get`.
    Route {
        name: "/artist/detail",
        upstream: "/api/artist/head/info/get",
        protocol: Protocol::Eapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    Route {
        name: "/artist/album",
        upstream: "/api/artist/albums/{id}",
        protocol: Protocol::Weapi,
        id_in_path: true,
        kind: RouteKind::Plain,
    },
    Route {
        name: "/artist/top/song",
        upstream: "/api/artist/top/song",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    Route {
        name: "/artist/songs",
        upstream: "/api/v1/artist/songs",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    // --- Song ---
    Route {
        name: "/song/detail",
        upstream: "/api/v3/song/detail",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    Route {
        name: "/song/url/v1",
        upstream: "/api/song/enhance/player/url/v1",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    Route {
        name: "/song/chorus",
        upstream: "/api/song/chorus",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    Route {
        name: "/song/copyright/rcmd",
        upstream: "/api/song/copyright/rcmd",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    Route {
        name: "/lyric/new",
        upstream: "/api/song/lyric/v1",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    // --- Search / radio / recommend ---
    Route {
        name: "/cloudsearch",
        upstream: "/api/cloudsearch/pc",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    Route {
        name: "/personal_fm",
        upstream: "/api/v1/radio/get",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    Route {
        name: "/recommend/songs",
        upstream: "/api/v3/discovery/recommend/songs",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    Route {
        name: "/recommend/songs/dislike",
        upstream: "/api/v2/discovery/recommend/dislike",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    Route {
        name: "/history/recommend/songs",
        upstream: "/api/discovery/recommend/songs/history/recent",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    Route {
        name: "/history/recommend/songs/detail",
        upstream: "/api/discovery/recommend/songs/history/detail",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    Route {
        name: "/personalized",
        upstream: "/api/personalized/playlist",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
    Route {
        name: "/fm_trash",
        upstream: "/api/radio/trash/add",
        protocol: Protocol::Weapi,
        id_in_path: false,
        kind: RouteKind::Plain,
    },
];

pub fn find_route(path: &str) -> Option<&'static Route> {
    ROUTES.iter().find(|r| r.name == path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_frontend_routes_are_registered() {
        let frontend = [
            "/register/anonimous",
            "/login/qr/key",
            "/login/qr/create",
            "/login/qr/check",
            "/login/status",
            "/logout",
            "/user/account",
            "/like",
            "/likelist",
            "/user/playlist",
            "/user/cloud",
            "/user/cloud/detail",
            "/cloud/lyric/get",
            "/playlist/detail",
            "/playlist/track/all",
            "/playlist/tracks",
            "/playlist/subscribe",
            "/playlist/detail/dynamic",
            "/album",
            "/album/sublist",
            "/album/sub",
            "/album/detail/dynamic",
            "/artist/detail",
            "/artist/album",
            "/artist/top/song",
            "/artist/songs",
            "/song/detail",
            "/song/url/v1",
            "/song/chorus",
            "/song/copyright/rcmd",
            "/lyric/new",
            "/cloudsearch",
            "/personal_fm",
            "/recommend/songs",
            "/recommend/songs/dislike",
            "/history/recommend/songs",
            "/history/recommend/songs/detail",
            "/personalized",
            "/fm_trash",
        ];
        for path in frontend {
            assert!(
                find_route(path).is_some(),
                "missing route for {path} (check src/services/netease.ts)"
            );
        }
        assert_eq!(ROUTES.len(), frontend.len());
    }

    #[test]
    fn only_xeapi_route_is_anonymous_register() {
        assert_eq!(
            find_route("/register/anonimous").unwrap().protocol,
            Protocol::Xeapi
        );
        for route in ROUTES {
            if route.name != "/register/anonimous" {
                assert_ne!(
                    route.protocol,
                    Protocol::Xeapi,
                    "{} must not be xeapi",
                    route.name
                );
            }
        }
    }

    #[test]
    fn route_paths_unique() {
        let mut seen = std::collections::HashSet::new();
        for route in ROUTES {
            assert!(seen.insert(route.name), "duplicate route {}", route.name);
        }
    }
}
