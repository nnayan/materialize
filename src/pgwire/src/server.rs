// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use async_trait::async_trait;
use mz_frontegg_auth::Authentication as FronteggAuthentication;
use mz_ore::netio::AsyncReady;
use mz_sql::session::vars::ConnectionCounter;
use openssl::ssl::{Ssl, SslContext};
use tokio::io::{self, AsyncRead, AsyncWrite, AsyncWriteExt, Interest, ReadBuf, Ready};
use tokio_openssl::SslStream;
use tracing::trace;

use crate::codec::{self, FramedConn, ACCEPT_SSL_ENCRYPTION, REJECT_ENCRYPTION};
use crate::message::FrontendStartupMessage;
use crate::metrics::{Metrics, MetricsConfig};
use crate::protocol;

/// Configures a [`Server`].
#[derive(Debug)]
pub struct Config {
    /// A client for the adapter with which the server will communicate.
    pub adapter_client: mz_adapter::Client,
    /// The TLS configuration for the server.
    ///
    /// If not present, then TLS is not enabled, and clients requests to
    /// negotiate TLS will be rejected.
    pub tls: Option<TlsConfig>,
    /// The Frontegg authentication configuration.
    ///
    /// If present, Frontegg authentication is enabled, and users may present
    /// a valid Frontegg API token as a password to authenticate. Otherwise,
    /// password authentication is disabled.
    pub frontegg: Option<FronteggAuthentication>,
    /// The registry entries that the pgwire server uses to report metrics.
    pub metrics: MetricsConfig,
    /// Whether this is an internal server that permits access to restricted
    /// system resources.
    pub internal: bool,
    /// Global connection limit and count
    pub active_connection_count: Arc<Mutex<ConnectionCounter>>,
}

/// Configures a server's TLS encryption and authentication.
#[derive(Clone, Debug)]
pub struct TlsConfig {
    /// The SSL context used to manage incoming TLS negotiations.
    pub context: SslContext,
    /// The TLS mode.
    pub mode: TlsMode,
}

/// Specifies how strictly to enforce TLS encryption.
#[derive(Debug, Clone, Copy)]
pub enum TlsMode {
    /// Allow TLS encryption.
    Allow,
    /// Require that clients negotiate TLS encryption.
    Require,
}

/// A server that communicates with clients via the pgwire protocol.
pub struct Server {
    tls: Option<TlsConfig>,
    adapter_client: mz_adapter::Client,
    frontegg: Option<FronteggAuthentication>,
    metrics: Metrics,
    internal: bool,
    active_connection_count: Arc<Mutex<ConnectionCounter>>,
}

impl Server {
    /// Constructs a new server.
    pub fn new(config: Config) -> Server {
        Server {
            tls: config.tls,
            adapter_client: config.adapter_client,
            frontegg: config.frontegg,
            metrics: Metrics::new(config.metrics, config.internal),
            internal: config.internal,
            active_connection_count: config.active_connection_count,
        }
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub fn handle_connection<A>(
        &self,
        conn: A,
    ) -> impl Future<Output = Result<(), anyhow::Error>> + 'static + Send
    where
        A: AsyncRead + AsyncWrite + AsyncReady + Send + Sync + Unpin + fmt::Debug + 'static,
    {
        let mut adapter_client = self.adapter_client.clone();
        let frontegg = self.frontegg.clone();
        let tls = self.tls.clone();
        let internal = self.internal;
        let metrics = self.metrics.clone();
        let active_connection_count = Arc::clone(&self.active_connection_count);
        async move {
            let result = (|| {
                async move {
                    let conn_id = adapter_client.new_conn_id()?;
                    let mut conn = Conn::Unencrypted(conn);
                    loop {
                        let message = codec::decode_startup(&mut conn).await?;

                        match &message {
                            Some(message) => trace!("cid={} recv={:?}", conn_id, message),
                            None => trace!("cid={} recv=<eof>", conn_id),
                        }

                        conn = match message {
                            // Clients sometimes hang up during the startup sequence, e.g.
                            // because they receive an unacceptable response to an
                            // `SslRequest`. This is considered a graceful termination.
                            None => return Ok(()),

                            Some(FrontendStartupMessage::Startup { version, params }) => {
                                let mut conn = FramedConn::new(conn_id.clone(), conn);
                                protocol::run(protocol::RunParams {
                                    tls_mode: tls.as_ref().map(|tls| tls.mode),
                                    adapter_client,
                                    conn: &mut conn,
                                    version,
                                    params,
                                    frontegg: frontegg.as_ref(),
                                    internal,
                                    active_connection_count,
                                })
                                .await?;
                                conn.flush().await?;
                                return Ok(());
                            }

                            Some(FrontendStartupMessage::CancelRequest {
                                conn_id,
                                secret_key,
                            }) => {
                                adapter_client.cancel_request(conn_id, secret_key);
                                // For security, the client is not told whether the cancel
                                // request succeeds or fails.
                                return Ok(());
                            }

                            Some(FrontendStartupMessage::SslRequest) => match (conn, &tls) {
                                (Conn::Unencrypted(mut conn), Some(tls)) => {
                                    trace!("cid={} send=AcceptSsl", conn_id);
                                    conn.write_all(&[ACCEPT_SSL_ENCRYPTION]).await?;
                                    let mut ssl_stream =
                                        SslStream::new(Ssl::new(&tls.context)?, conn)?;
                                    if let Err(e) = Pin::new(&mut ssl_stream).accept().await {
                                        let _ = ssl_stream.get_mut().shutdown().await;
                                        return Err(e.into());
                                    }
                                    Conn::Ssl(ssl_stream)
                                }
                                (mut conn, _) => {
                                    trace!("cid={} send=RejectSsl", conn_id);
                                    conn.write_all(&[REJECT_ENCRYPTION]).await?;
                                    conn
                                }
                            },

                            Some(FrontendStartupMessage::GssEncRequest) => {
                                trace!("cid={} send=RejectGssEnc", conn_id);
                                conn.write_all(&[REJECT_ENCRYPTION]).await?;
                                conn
                            }
                        }
                    }
                }
            })()
            .await;
            let status = match result {
                Ok(()) => "success",
                Err(_) => "error",
            };
            metrics.connection_status(status).inc();
            result
        }
    }
}

#[derive(Debug)]
pub enum Conn<A> {
    Unencrypted(A),
    Ssl(SslStream<A>),
}

impl<A> AsyncRead for Conn<A>
where
    A: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut ReadBuf,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Conn::Unencrypted(inner) => Pin::new(inner).poll_read(cx, buf),
            Conn::Ssl(inner) => Pin::new(inner).poll_read(cx, buf),
        }
    }
}

impl<A> AsyncWrite for Conn<A>
where
    A: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Conn::Unencrypted(inner) => Pin::new(inner).poll_write(cx, buf),
            Conn::Ssl(inner) => Pin::new(inner).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Conn::Unencrypted(inner) => Pin::new(inner).poll_flush(cx),
            Conn::Ssl(inner) => Pin::new(inner).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Conn::Unencrypted(inner) => Pin::new(inner).poll_shutdown(cx),
            Conn::Ssl(inner) => Pin::new(inner).poll_shutdown(cx),
        }
    }
}

#[async_trait]
impl<A> AsyncReady for Conn<A>
where
    A: AsyncRead + AsyncWrite + AsyncReady + Sync + Unpin,
{
    async fn ready(&self, interest: Interest) -> io::Result<Ready> {
        match self {
            Conn::Unencrypted(inner) => inner.ready(interest).await,
            Conn::Ssl(inner) => inner.ready(interest).await,
        }
    }
}
