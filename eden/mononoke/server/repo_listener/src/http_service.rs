/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::{Context, Error, Result};
use futures::future::{BoxFuture, FutureExt};
use gotham::ConnectedGothamService;
use gotham_ext::socket_data::TlsSocketData;
use http::{HeaderMap, HeaderValue, Method, Request, Response, Uri};
use hyper::{service::Service, Body};
use sha1::{Digest, Sha1};
use slog::{debug, error, Logger};
use sshrelay::Metadata;
use std::io::Cursor;
use std::marker::PhantomData;
use std::str::FromStr;
use std::sync::{atomic::Ordering, Arc};
use std::task;
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tunables::tunables;

use crate::connection_acceptor::{
    self, AcceptedConnection, Acceptor, ChannelConn, FramedConn, MononokeStream,
};

const HEADER_CLIENT_DEBUG: &str = "x-client-debug";
const HEADER_WEBSOCKET_KEY: &str = "sec-websocket-key";
const HEADER_WEBSOCKET_ACCEPT: &str = "sec-websocket-accept";
const HEADER_MONONOKE_HOST: &str = "x-mononoke-host";

// See https://tools.ietf.org/html/rfc6455#section-1.3
const WEBSOCKET_MAGIC_KEY: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

#[derive(Error, Debug)]
pub enum HttpError {
    #[error("Bad request")]
    BadRequest(#[source] Error),

    #[error("Method not acceptable")]
    NotAcceptable,

    #[error("Not found")]
    NotFound,

    #[error("Internal server error")]
    InternalServerError(#[source] Error),
}

impl HttpError {
    pub fn internal(e: impl Into<Error>) -> Self {
        Self::InternalServerError(e.into())
    }

    pub fn http_response(&self) -> http::Result<Response<Body>> {
        let status = match self {
            Self::BadRequest(..) => http::StatusCode::BAD_REQUEST,
            Self::NotAcceptable => http::StatusCode::NOT_ACCEPTABLE,
            Self::NotFound => http::StatusCode::NOT_FOUND,
            Self::InternalServerError(..) => http::StatusCode::INTERNAL_SERVER_ERROR,
        };

        let body = match self {
            Self::BadRequest(ref e) => Body::from(format!("{:#}", e)),
            Self::NotAcceptable => Body::empty(),
            Self::NotFound => Body::empty(),
            Self::InternalServerError(ref e) => Body::from(format!("{:#}", e)),
        };

        Response::builder().status(status).body(body)
    }
}

pub struct MononokeHttpService<S> {
    pub conn: AcceptedConnection,
    sock: PhantomData<S>,
}

impl<S> MononokeHttpService<S> {
    pub fn new(conn: AcceptedConnection) -> Self {
        Self {
            conn,
            sock: PhantomData,
        }
    }
}

impl<S> Clone for MononokeHttpService<S> {
    fn clone(&self) -> Self {
        Self {
            conn: self.conn.clone(),
            sock: PhantomData,
        }
    }
}

impl<S> MononokeHttpService<S>
where
    S: MononokeStream,
{
    async fn handle(
        &self,
        req: http::request::Parts,
        body: Body,
    ) -> Result<Response<Body>, HttpError> {
        let upgrade = req
            .headers
            .get(http::header::UPGRADE)
            .as_ref()
            .map(|h| h.to_str())
            .transpose()
            .with_context(|| {
                // NOTE: We're just stringifying here: the borrow is fine.
                #[allow(clippy::borrow_interior_mutable_const)]
                let header = &http::header::UPGRADE;
                format!("Invalid header: {}", header)
            })
            .map_err(HttpError::BadRequest)?;

        if upgrade == Some("websocket") {
            return self
                .handle_websocket_request(&req.uri, &req.headers, body)
                .await;
        }

        if req.uri.path() == "/netspeedtest" {
            return crate::netspeedtest::handle(req.method, &req.headers, body).await;
        }

        if let Some(path) = req.uri.path().strip_prefix("/control") {
            return self.handle_control_request(req.method, path).await;
        }

        if req.method == Method::GET && (req.uri.path() == "/" || req.uri.path() == "/health_check")
        {
            let res = if self.acceptor().will_exit.load(Ordering::Relaxed) {
                "EXITING"
            } else {
                "I_AM_ALIVE"
            };

            let res = Response::builder()
                .status(http::StatusCode::OK)
                .body(res.into())
                .map_err(HttpError::internal)?;

            return Ok(res);
        }

        let edenapi_path_and_query = req
            .uri
            .path_and_query()
            .as_ref()
            .and_then(|pq| pq.as_str().strip_prefix("/edenapi"));

        if let Some(edenapi_path_and_query) = edenapi_path_and_query {
            let pq = http::uri::PathAndQuery::from_str(edenapi_path_and_query)
                .context("Error translating EdenAPI request path")
                .map_err(HttpError::internal)?;
            return self.handle_eden_api_request(req, pq, body).await;
        }

        Err(HttpError::NotFound)
    }

    async fn handle_websocket_request(
        &self,
        uri: &Uri,
        headers: &HeaderMap<HeaderValue>,
        body: Body,
    ) -> Result<Response<Body>, HttpError> {
        let reponame = uri.path().trim_matches('/').to_string();

        let websocket_key = calculate_websocket_accept(headers);

        let res = Response::builder()
            .status(http::StatusCode::SWITCHING_PROTOCOLS)
            .header(http::header::CONNECTION, "upgrade")
            .header(http::header::UPGRADE, "websocket")
            .header(HEADER_WEBSOCKET_ACCEPT, websocket_key)
            .body(Body::empty())
            .map_err(HttpError::internal)?;

        let metadata = try_convert_headers_to_metadata(self.conn.is_trusted, &headers)
            .await
            .context("Invalid metadata")
            .map_err(HttpError::BadRequest)?;

        let debug = headers.get(HEADER_CLIENT_DEBUG).is_some();

        let this = self.clone();

        let fut = async move {
            let io = body
                .on_upgrade()
                .await
                .context("Failed to upgrade connection")?;

            // NOTE: We unwrap() here because we explicitly parameterize the MononokeHttpService
            // over its socket type. If we get it wrong then that'd be a deterministic failure that
            // would show up in tests.
            let hyper::upgrade::Parts { io, read_buf, .. } = io.downcast::<S>().unwrap();

            let (rx, tx) = tokio::io::split(io);
            let rx = AsyncReadExt::chain(Cursor::new(read_buf), rx);

            let conn = FramedConn::setup(rx, tx);
            let channels = ChannelConn::setup(conn);

            connection_acceptor::handle_wireproto(this.conn, channels, reponame, metadata, debug)
                .await
                .context("Failed to handle_wireproto")?;

            Result::<_, Error>::Ok(())
        };

        self.conn
            .pending
            .spawn_task(fut, "Failed to handle websocket channel");

        Ok(res)
    }

    async fn handle_control_request(
        &self,
        method: Method,
        path: &str,
    ) -> Result<Response<Body>, HttpError> {
        if method != Method::POST {
            return Err(HttpError::NotAcceptable);
        }

        let ok = Response::builder()
            .status(http::StatusCode::OK)
            .body(Body::empty())
            .map_err(HttpError::internal)?;

        if path == "/drop_bookmarks_cache" {
            for handler in self.acceptor().repo_handlers.values() {
                handler.repo.blobrepo().bookmarks().drop_caches();
            }

            return Ok(ok);
        }

        Err(HttpError::NotFound)
    }

    async fn handle_eden_api_request(
        &self,
        mut req: http::request::Parts,
        pq: http::uri::PathAndQuery,
        body: Body,
    ) -> Result<Response<Body>, HttpError> {
        if tunables().get_disable_http_service_edenapi() {
            let res = Response::builder()
                .status(http::StatusCode::SERVICE_UNAVAILABLE)
                .body("EdenAPI service is killswitched".into())
                .map_err(HttpError::internal)?;
            return Ok(res);
        }

        let mut uri_parts = req.uri.into_parts();

        uri_parts.path_and_query = Some(pq);

        req.uri = Uri::from_parts(uri_parts)
            .context("Error translating EdenAPI request")
            .map_err(HttpError::internal)?;

        let socket_data = if self.conn.is_trusted {
            TlsSocketData::trusted_proxy()
        } else {
            TlsSocketData::authenticated_identities((*self.conn.identities).clone())
        };

        let mut gotham = ConnectedGothamService::connect(
            Arc::new(self.acceptor().edenapi.clone()),
            self.conn.pending.addr,
            socket_data,
        );

        return gotham
            .call(Request::from_parts(req, body))
            .await
            .map_err(HttpError::internal);
    }

    fn acceptor(&self) -> &Acceptor {
        &self.conn.pending.acceptor
    }

    fn logger(&self) -> &Logger {
        &self.acceptor().logger
    }
}

impl<S> Service<Request<Body>> for MononokeHttpService<S>
where
    S: MononokeStream,
{
    type Response = Response<Body>;
    type Error = http::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut task::Context<'_>) -> task::Poll<Result<(), Self::Error>> {
        task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let this = self.clone();

        async move {
            let (req, body) = req.into_parts();

            let method = req.method.clone();
            let uri = req.uri.clone();
            debug!(this.logger(), "{} {}", method, uri);

            let res = this
                .handle(req, body)
                .await
                .and_then(|mut res| {
                    match HeaderValue::from_str(this.conn.pending.acceptor.server_hostname.as_str())
                    {
                        Ok(header) => {
                            res.headers_mut().insert(HEADER_MONONOKE_HOST, header);
                        }
                        Err(e) => {
                            error!(
                                this.logger(),
                                "http service error: can't set {} header: {}",
                                HEADER_MONONOKE_HOST,
                                e
                            );
                        }
                    };
                    Ok(res)
                })
                .or_else(|e| {
                    error!(
                        this.logger(),
                        "http service error: {} {}: {:#}", method, uri, e
                    );

                    e.http_response()
                });

            // NOTE: If we fail to even generate the response here, this will crash
            // serve_connection in Hyper, so we don't actually need to log this here.
            res
        }
        .boxed()
    }
}

// See https://tools.ietf.org/html/rfc6455#section-1.3
fn calculate_websocket_accept(headers: &HeaderMap<HeaderValue>) -> String {
    let mut sha1 = Sha1::new();

    // This is OK to fall back to empty, because we only need to give
    // this header, if it's asked for. In case of hg<->mononoke with
    // no Proxygen in between, this header will be missing and the result
    // ignored.
    if let Some(header) = headers.get(HEADER_WEBSOCKET_KEY) {
        sha1.input(header.as_ref());
    }
    sha1.input(WEBSOCKET_MAGIC_KEY.as_bytes());
    let hash: [u8; 20] = sha1.result().into();
    base64::encode(&hash)
}

#[cfg(fbcode_build)]
async fn try_convert_headers_to_metadata(
    is_trusted: bool,
    headers: &HeaderMap<HeaderValue>,
) -> Result<Option<Metadata>> {
    use percent_encoding::percent_decode;
    use permission_checker::MononokeIdentity;
    use session_id::generate_session_id;
    use sshrelay::Priority;
    use std::net::IpAddr;

    const HEADER_ENCODED_CLIENT_IDENTITY: &str = "x-fb-validated-client-encoded-identity";
    const HEADER_CLIENT_IP: &str = "tfb-orig-client-ip";

    if !is_trusted {
        return Ok(None);
    }

    if let (Some(encoded_identities), Some(client_address)) = (
        headers.get(HEADER_ENCODED_CLIENT_IDENTITY),
        headers.get(HEADER_CLIENT_IP),
    ) {
        let json_identities = percent_decode(encoded_identities.as_ref())
            .decode_utf8()
            .context("Invalid encoded identities")?;
        let identities = MononokeIdentity::try_from_json_encoded(&json_identities)
            .context("Invalid identities")?;
        let ip_addr = client_address
            .to_str()?
            .parse::<IpAddr>()
            .context("Invalid IP Address")?;

        // In the case of HTTP proxied/trusted requests we only have the
        // guarantee that we can trust the forwarded credentials. Beyond
        // this point we can't trust anything else, ACL checks have not
        // been performed, so set 'is_trusted' to 'false' here to enforce
        // further checks.
        Ok(Some(
            Metadata::new(
                Some(&generate_session_id().to_string()),
                false,
                identities,
                Priority::Default,
                headers.contains_key(HEADER_CLIENT_DEBUG),
                Some(ip_addr),
            )
            .await,
        ))
    } else {
        Ok(None)
    }
}

#[cfg(not(fbcode_build))]
async fn try_convert_headers_to_metadata(
    _is_trusted: bool,
    _headers: &HeaderMap<HeaderValue>,
) -> Result<Option<Metadata>> {
    Ok(None)
}