use crate::proxy::http::{
    glue::{Body, HyperServerSvc},
    h2::Settings as H2Settings,
    trace, upgrade, Version as HttpVersion,
};
use crate::transport::{
    io::{self, BoxedIo, Peekable},
    tls,
};
use crate::{
    drain,
    proxy::{core::Accept, detect},
    svc::{NewService, Service, ServiceExt},
    Error,
};
use async_trait::async_trait;
use futures::TryFutureExt;
use http;
use hyper;
use indexmap::IndexSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tracing::{info_span, trace};
use tracing_futures::Instrument;

#[derive(Clone, Debug)]
pub struct Protocol {
    pub http: Option<HttpVersion>,
    pub tls: tls::accept::Meta,
}

pub type Connection = (Protocol, BoxedIo);

#[derive(Clone, Debug)]
pub struct ProtocolDetect {
    capacity: usize,
    skip_ports: Arc<IndexSet<u16>>,
}

impl ProtocolDetect {
    const PEEK_CAPACITY: usize = 8192;

    pub fn new(skip_ports: Arc<IndexSet<u16>>) -> Self {
        ProtocolDetect {
            skip_ports,
            capacity: Self::PEEK_CAPACITY,
        }
    }
}

#[async_trait]
impl detect::Detect<tls::accept::Meta> for ProtocolDetect {
    type Target = Protocol;
    type Error = io::Error;

    async fn detect(
        &self,
        tls: tls::accept::Meta,
        io: BoxedIo,
    ) -> Result<(Self::Target, BoxedIo), Self::Error> {
        let port = tls.addrs.target_addr().port();

        // Skip detection if the port is in the configured set.
        if self.skip_ports.contains(&port) {
            let proto = Protocol { tls, http: None };
            return Ok::<_, Self::Error>((proto, io));
        }

        // Otherwise, attempt to peek the client connection to determine the protocol.
        // Currently, we only check for an HTTP prefix.
        let peek = io.peek(self.capacity).await?;
        let http = HttpVersion::from_prefix(peek.prefix().as_ref());
        let proto = Protocol { tls, http };
        Ok((proto, BoxedIo::new(peek)))
    }
}

/// A protocol-transparent Server!
///
/// As TCP streams are passed to `Server::serve`, the following occurs:
///
/// *   A `Source` is created to describe the accepted connection.
///
/// *  If the original destination address's port is not specified in
///    `disable_protocol_detection_ports`, then data received on the connection is
///    buffered until the server can determine whether the streams begins with a
///    HTTP/1 or HTTP/2 preamble.
///
/// *  If the stream is not determined to be HTTP, then the original destination
///    address is used to transparently forward the TCP stream. A `C`-typed
///    `Connect` `Stack` is used to build a connection to the destination (i.e.,
///    instrumented with telemetry, etc).
///
/// *  Otherwise, an `H`-typed `Service` is used to build a service that
///    can route HTTP  requests for the `tls::accept::Meta`.
pub struct Server<F, H, B>
where
    H: NewService<tls::accept::Meta>,
    H::Service: Service<http::Request<Body>, Response = http::Response<B>>,
{
    http: hyper::server::conn::Http<trace::Executor>,
    forward_tcp: F,
    make_http: H,
    drain: drain::Watch,
}

impl<F, H, B> Server<F, H, B>
where
    H: NewService<tls::accept::Meta>,
    H::Service: Service<http::Request<Body>, Response = http::Response<B>>,
    Self: Accept<Connection>,
{
    pub fn new(forward_tcp: F, make_http: H, h2: H2Settings, drain: drain::Watch) -> Self {
        let mut http = hyper::server::conn::Http::new().with_executor(trace::Executor::new());
        http.http2_adaptive_window(true)
            .http2_initial_stream_window_size(h2.initial_stream_window_size)
            .http2_initial_connection_window_size(h2.initial_connection_window_size);

        Self {
            http,
            forward_tcp,
            make_http,
            drain,
        }
    }
}

impl<F, H, B> Service<Connection> for Server<F, H, B>
where
    F: Accept<(tls::accept::Meta, BoxedIo)> + Clone + Send + 'static,
    F::Future: Send + 'static,
    F::ConnectionFuture: Send + 'static,
    H: NewService<tls::accept::Meta> + Send + 'static,
    H::Service: Service<http::Request<Body>, Response = http::Response<B>, Error = Error>
        + Unpin
        + Send
        + 'static,
    <H::Service as Service<http::Request<Body>>>::Future: Send + 'static,
    B: hyper::body::HttpBody + Default + Send + 'static,
    B::Error: Into<Error>,
    B::Data: Send + 'static,
{
    type Response = Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'static>>;
    type Error = Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(().into()))
    }

    /// Handle a new connection.
    ///
    /// This will peek on the connection for the first bytes to determine
    /// what protocol the connection is speaking. From there, the connection
    /// will be mapped into respective services, and spawned into an
    /// executor.
    fn call(&mut self, (proto, io): Connection) -> Self::Future {
        let http_version = match proto.http {
            Some(http) => http,
            None => {
                trace!("did not detect protocol; forwarding TCP");
                let drain = self.drain.clone();
                let forward = self.forward_tcp.clone();
                return Box::pin(async move {
                    let conn = forward
                        .into_service()
                        .oneshot((proto.tls, io))
                        .await
                        .map_err(Into::into)?;

                    let rsp: Self::Response = Box::pin(
                        drain
                            .ignore_signal()
                            .release_after(conn)
                            .map_err(Into::into),
                    );

                    Ok(rsp)
                });
            }
        };

        let http_svc = self.make_http.new_service(proto.tls);

        let mut builder = self.http.clone();
        let drain = self.drain.clone();
        Box::pin(async move {
            let rsp: Self::Response = Box::pin(async move {
                match http_version {
                    HttpVersion::Http1 => {
                        // Enable support for HTTP upgrades (CONNECT and websockets).
                        let svc = upgrade::Service::new(http_svc, drain.clone());
                        let conn = builder
                            .http1_only(true)
                            .serve_connection(io, HyperServerSvc::new(svc))
                            .with_upgrades();
                        drain
                            .watch(conn, |conn| Pin::new(conn).graceful_shutdown())
                            .instrument(info_span!("h1"))
                            .await?;
                    }

                    HttpVersion::H2 => {
                        let conn = builder
                            .http2_only(true)
                            .serve_connection(io, HyperServerSvc::new(http_svc));
                        drain
                            .watch(conn, |conn| Pin::new(conn).graceful_shutdown())
                            .instrument(info_span!("h2"))
                            .await?;
                    }
                }

                Ok(())
            });

            Ok(rsp)
        })
    }
}

impl<F, H, B> Clone for Server<F, H, B>
where
    F: Clone,
    H: NewService<tls::accept::Meta> + Clone,
    H::Service: Service<http::Request<Body>, Response = http::Response<B>>,
    B: hyper::body::HttpBody,
{
    fn clone(&self) -> Self {
        Self {
            http: self.http.clone(),
            forward_tcp: self.forward_tcp.clone(),
            make_http: self.make_http.clone(),
            drain: self.drain.clone(),
        }
    }
}
