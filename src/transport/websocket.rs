use core::result::Result;
use std::io::{Error, ErrorKind};
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{ready, Context, Poll};

use anyhow::{anyhow, Result as AnyhowResult};
use async_trait::async_trait;
use futures_core::stream::Stream;
use futures_sink::Sink;
use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};
use tokio_tungstenite::{accept_async_with_config, client_async_with_config, WebSocketStream};
use tokio_util::io::StreamReader;
use url::Url;
use bytes::{Bytes, BytesMut};

use super::maybe_tls::{MaybeTLSStream, MaybeTLSTransport};
use super::{AddrMaybeCached, SocketOpts, Transport};
use crate::config::TransportConfig;

#[derive(Debug)]
struct StreamWrapper {
    inner: WebSocketStream<MaybeTLSStream>,
    write_buf: BytesMut,
    max_size: usize,
}

impl Stream for StreamWrapper {
    type Item = Result<Bytes, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(Error::new(ErrorKind::Other, err)))),
            Poll::Ready(Some(Ok(res))) => {
                if let Message::Binary(b) = res {
                    Poll::Ready(Some(Ok(Bytes::from(b))))
                } else {
                    Poll::Ready(Some(Err(Error::new(
                        ErrorKind::InvalidData,
                        "unexpected frame",
                    ))))
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct WebsocketTunnel {
    inner: StreamReader<StreamWrapper, Bytes>,
}

impl AsyncRead for WebsocketTunnel {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncBufRead for WebsocketTunnel {
    fn poll_fill_buf(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<&[u8]>> {
        Pin::new(&mut self.inner).poll_fill_buf(cx)
    }

    fn consume(mut self: Pin<&mut Self>, amt: usize) {
        Pin::new(&mut self.inner).consume(amt)
    }
}

impl AsyncWrite for WebsocketTunnel {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let sw = &mut self.inner.get_mut();
        sw.write_buf.extend_from_slice(buf);

        while !sw.write_buf.is_empty() {
            ready!(Pin::new(&mut sw.inner).poll_ready(cx)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)))?;

            let chunk_size = std::cmp::min(sw.write_buf.len(), sw.max_size);
            let chunk = sw.write_buf.split_to(chunk_size).freeze();

            Pin::new(&mut sw.inner)
                .start_send(Message::Binary(chunk.to_vec()))
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        }

        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        let sw = &mut self.inner.get_mut();

        while !sw.write_buf.is_empty() {
            ready!(Pin::new(&mut sw.inner).poll_ready(cx)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)))?;

            let chunk_size = std::cmp::min(sw.write_buf.len(), sw.max_size);
            let chunk = sw.write_buf.split_to(chunk_size).freeze();

            Pin::new(&mut sw.inner)
                .start_send(Message::Binary(chunk.to_vec()))
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        }

        Pin::new(&mut sw.inner)
            .poll_flush(cx)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.inner.get_mut().inner)
            .poll_close(cx)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
    }
}

#[derive(Debug)]
pub struct WebsocketTransport {
    sub: MaybeTLSTransport,
    conf: WebSocketConfig,
}

#[async_trait]
impl Transport for WebsocketTransport {
    type Acceptor = TcpListener;
    type RawStream = TcpStream;
    type Stream = WebsocketTunnel;

    fn new(config: &TransportConfig) -> AnyhowResult<Self> {
        let wsconfig = config
            .websocket
            .as_ref()
            .ok_or_else(|| anyhow!("Missing websocket config"))?;

        let conf = WebSocketConfig {
            write_buffer_size: 256 * 1024,
            max_message_size: Some(16 * 1024 * 1024),
            max_frame_size: Some(1024 * 1024), // 增大帧大小到1MB
            accept_unmasked_frames: true,
            ..WebSocketConfig::default()
        };
        let sub = MaybeTLSTransport::new_explicit(wsconfig.tls, config)?;
        Ok(WebsocketTransport { sub, conf })
    }

    fn hint(conn: &Self::Stream, opt: SocketOpts) {
        opt.apply(conn.inner.get_ref().inner.get_ref().get_tcpstream())
    }

    async fn bind<A: ToSocketAddrs + Send + Sync>(
        &self,
        addr: A,
    ) -> AnyhowResult<Self::Acceptor> {
        self.sub.bind(addr).await
    }

    async fn accept(&self, a: &Self::Acceptor) -> AnyhowResult<(Self::RawStream, SocketAddr)> {
        self.sub.accept(a).await
    }

    async fn handshake(&self, conn: Self::RawStream) -> AnyhowResult<Self::Stream> {
        conn.set_nodelay(true)?;
        let tstream = self.sub.handshake(conn).await?;
        let wsstream = accept_async_with_config(tstream, Some(self.conf)).await?;
        Ok(WebsocketTunnel {
            inner: StreamReader::new(StreamWrapper {
                inner: wsstream,
                write_buf: BytesMut::new(),
                max_size: 256 * 1024,
            }),
        })
    }

    async fn connect(&self, addr: &AddrMaybeCached) -> AnyhowResult<Self::Stream> {
        let u = format!("ws://{}", &addr.addr.as_str());
        let url = Url::parse(&u).unwrap();
        let tstream = self.sub.connect(addr).await?;
        let (wsstream, _) = client_async_with_config(url, tstream, Some(self.conf))
            .await
            .map_err(|e| anyhow!("WebSocket connection failed: {}", e))?;
        Ok(WebsocketTunnel {
            inner: StreamReader::new(StreamWrapper {
                inner: wsstream,
                write_buf: BytesMut::new(),
                max_size: 256 * 1024,
            }),
        })
    }
}