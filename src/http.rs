// SPDX-License-Identifier: GPL-3.0-or-later

//! Blocking HTTP server for scripts, home automation and Stream Deck buttons.
//!
//! Everything is `GET` and plain text: `ok`, an error line, or the same
//! `name value` lines `rsttherm measure` prints. Bad requests are 400, anything
//! the camera could not do (including having no frame yet) is 500.

use tiny_http::{Response, Server};

use crate::camera;
use crate::live::Live;

pub const DEFAULT_BIND: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8080;

/// Parsed separately from the server loop so routing is testable without a
/// socket or a camera.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    ListDevices,
    /// Optionally with one pixel, in sensor coordinates.
    Measure(Option<(usize, usize)>),
    Shutter,
}

pub fn parse_route(url: &str) -> Result<Route, String> {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    Ok(match segments.as_slice() {
        ["devices"] => Route::ListDevices,
        ["measure"] => match (query_param(query, "x"), query_param(query, "y")) {
            (None, None) => Route::Measure(None),
            (Some(x), Some(y)) => Route::Measure(Some((coord(x)?, coord(y)?))),
            _ => return Err("give both x and y, or neither".into()),
        },
        ["shutter"] => Route::Shutter,
        _ => return Err(format!("unknown path '{path}'")),
    })
}

fn coord(text: &str) -> Result<usize, String> {
    text.parse()
        .map_err(|_| format!("'{text}' is not a pixel coordinate"))
}

fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

pub fn execute(live: &Live, route: &Route) -> camera::Result<String> {
    match *route {
        Route::ListDevices => Ok(camera::list_devices()?
            .iter()
            .map(|device| format!("{device}\n"))
            .collect()),
        Route::Measure(point) => live.image()?.report(point),
        Route::Shutter => {
            live.shutter();
            Ok("ok\n".to_string())
        }
    }
}

fn respond(live: &Live, url: &str) -> (u16, String) {
    match parse_route(url) {
        Err(e) => (400, format!("{e}\n")),
        Ok(route) => match execute(live, &route) {
            Ok(body) => (200, body),
            Err(e) => (500, format!("{e}\n")),
        },
    }
}

pub fn serve(live: &Live, bind: &str, port: u16) -> camera::Result<()> {
    let server = Server::http((bind, port)).map_err(|e| format!("bind {bind}:{port}: {e}"))?;
    eprintln!("rsttherm: listening on http://{bind}:{port}");
    for request in server.incoming_requests() {
        let (code, body) = respond(live, request.url());
        let _ = request.respond(Response::from_string(body).with_status_code(code));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_every_endpoint() {
        let route = |url| parse_route(url).expect(url);
        assert_eq!(route("/devices"), Route::ListDevices);
        assert_eq!(route("/measure"), Route::Measure(None));
        assert_eq!(route("/measure?x=10&y=20"), Route::Measure(Some((10, 20))));
        assert_eq!(route("/shutter"), Route::Shutter);
    }

    #[test]
    fn rejects_bad_requests() {
        for bad in [
            "/",
            "/nope",
            "/shutter/now",
            "/measure?x=1",
            "/measure?x=a&y=1",
            "/measure?x=-1&y=1",
        ] {
            assert!(parse_route(bad).is_err(), "{bad:?} should be rejected");
        }
    }
}
