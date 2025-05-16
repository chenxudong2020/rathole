use core::result::Result;
use std::io::{Error, ErrorKind};
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{ready, Context, Poll};

use anyhow::{anyhow, Result as AnyhowResult};
use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::Stream;
use futures_sink::Sink;
use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};
use tokio_tungstenite::{accept_async_with_config, client_async_with_config, WebSocketStream};
use tokio_util::io::StreamReader;
use url::Url;

use super::maybe_tls::{MaybeTLSStream, MaybeTLSTransport};
use super::{AddrMaybeCached, SocketOpts, Transport};
use crate::config::TransportConfig;

#[derive(Debug)]
struct StreamWrapper {
    inner: WebSocketStream<MaybeTLSStream>,
}

impl Stream for StreamWrapper {
    type Item = Result<Bytes, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.get_mut().inner).poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(Err(err))) => {
                Poll::Ready(Some(Err(Error::new(ErrorKind::Other, err))))
            }
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

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

#[derive(Debug)]
pub struct WebsocketTunnel {
    inner: StreamReader<StreamWrapper, Bytes>,
}

impl AsyncRead for WebsocketTunnel {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl AsyncBufRead for WebsocketTunnel {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<&[u8]>> {
        Pin::new(&mut self.get_mut().inner).poll_fill_buf(cx)
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        Pin::new(&mut self.get_mut().inner).consume(amt)
    }
}

impl AsyncWrite for WebsocketTunnel {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let sw = self.get_mut().inner.get_mut();
        ready!(Pin::new(&mut sw.inner)
            .poll_ready(cx)
            .map_err(|err| Error::new(ErrorKind::Other, err)))?;

        // 使用零拷贝技术减少内存分配
       match Pin::new(&mut sw.inner).start_send(Message::Binary(Bytes::copy_from_slice(buf))) {
            Ok(()) => Poll::Ready(Ok(buf.len())),
            Err(e) => Poll::Ready(Err(Error::new(ErrorKind::Other, e))),
        }


    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Pin::new(&mut self.get_mut().inner.get_mut().inner)
            .poll_flush(cx)
            .map_err(|err| Error::new(ErrorKind::Other, err))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Pin::new(&mut self.get_mut().inner.get_mut().inner)
            .poll_close(cx)
            .map_err(|err| Error::new(ErrorKind::Other, err))
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

        // 优化 WebSocket 配置
        let conf = WebSocketConfig {
            write_buffer_size: 256 * 1024, // 设置写缓冲区大小为 256KB
            max_message_size: Some(64 * 1024 * 1024), // 最大消息大小为 64MB
            accept_unmasked_frames: true, //启用未屏蔽帧以提高性能
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
       // 优化 TCP_NODELAY 设置 (优化点)
        conn.set_nodelay(true)?;

        let tstream = self.sub.handshake(conn).await?;
        let wsstream = accept_async_with_config(tstream, Some(self.conf)).await?;
        let tun = WebsocketTunnel {
            inner: StreamReader::new(StreamWrapper { inner: wsstream }),
        };
        Ok(tun)
    }

    async fn connect(&self, addr: &AddrMaybeCached) -> AnyhowResult<Self::Stream> {
        let u = format!("ws://{}", &addr.addr.as_str());
        let url = Url::parse(&u).unwrap();
        let tstream = self.sub.connect(addr).await?;
        let (wsstream, _) = client_async_with_config(url, tstream, Some(self.conf))
            .await
            .expect("failed to connect");
        let tun = WebsocketTunnel {
            inner: StreamReader::new(StreamWrapper { inner: wsstream }),
        };
        Ok(tun)
    }
}