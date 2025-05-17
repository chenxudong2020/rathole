use core::result::Result;
use std::io::{Error, ErrorKind};
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll, ready}; // 引入 ready! 宏
use std::time::Duration;

use anyhow::{anyhow, Result as AnyhowResult};
use async_trait::async_trait;
use futures_core::stream::Stream;
use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};
use tokio_tungstenite::{accept_async_with_config, client_async_with_config, WebSocketStream};
use tokio_util::io::StreamReader;
use url::Url;
use bytes::{BytesMut};

use super::maybe_tls::{MaybeTLSStream, MaybeTLSTransport};
use super::{AddrMaybeCached, SocketOpts, Transport};
use crate::config::TransportConfig;

// 必须引入这些 trait 才能使用 split() / send()
use futures_util::{
    stream::StreamExt, // 提供 .split()
    sink::SinkExt,     // 提供 .send(), .flush()
};

#[derive(Debug)]
struct StreamWrapper {
    inner: SplitSink<WebSocketStream<MaybeTLSStream>, Message>,
    write_buf: BytesMut,
    max_size: usize,
}

impl StreamWrapper {
    fn new(stream: WebSocketStream<MaybeTLSStream>) -> Self {
        let (sink, _) = stream.split();
        StreamWrapper {
            inner: sink,
            write_buf: BytesMut::new(),
            max_size: 256 * 1024,
        }
    }

    async fn send_pending(&mut self) -> Result<(), Error> {
        if !self.write_buf.is_empty() {
            let data = std::mem::take(&mut self.write_buf);
            self.inner.send(Message::Binary(data.to_vec())).await.map_err(|e| {
                Error::new(ErrorKind::Other, e)
            })?;
        }
        Ok(())
    }
}

impl AsyncWrite for StreamWrapper {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let this = self.get_mut();
        this.write_buf.extend_from_slice(buf);

        if this.write_buf.len() >= this.max_size {
            ready!(Pin::new(&mut this.inner).poll_ready(cx).map_err(|err| Error::new(ErrorKind::Other, err)))?;
            let data = std::mem::take(&mut this.write_buf);
            match Pin::new(&mut this.inner).start_send(Message::Binary(data.to_vec())) {
                Ok(()) => Poll::Ready(Ok(buf.len())),
                Err(e) => Poll::Ready(Err(Error::new(ErrorKind::Other, e))),
            }
        } else {
            Poll::Ready(Ok(buf.len()))
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let this = self.get_mut();
        ready!(Pin::new(&mut this.inner).poll_ready(cx).map_err(|err| Error::new(ErrorKind::Other, err)))?;
        if !this.write_buf.is_empty() {
            let data = std::mem::take(&mut this.write_buf);
            if let Err(e) = Pin::new(&mut this.inner).start_send(Message::Binary(data.to_vec())) {
                return Poll::Ready(Err(Error::new(ErrorKind::Other, e)));
            }
        }
        Pin::new(&mut this.inner).poll_flush(cx).map_err(|err| Error::new(ErrorKind::Other, err))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let this = self.get_mut();
        ready!(Pin::new(&mut this.inner).poll_ready(cx).map_err(|err| Error::new(ErrorKind::Other, err)))?;
        Pin::new(&mut this.inner).poll_close(cx).map_err(|err| Error::new(ErrorKind::Other, err))
    }
}

#[derive(Debug)]
pub struct WebsocketTunnel {
    inner: StreamReader<StreamWrapper, BytesMut>,
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
            accept_unmasked_frames: true,
            max_frame_size: Some(64 * 1024),
            ..WebSocketConfig::default()
        };

        let sub = MaybeTLSTransport::new_explicit(wsconfig.tls, config)?;
        Ok(WebsocketTransport { sub, conf })
    }

    fn hint(conn: &Self::Stream, opt: SocketOpts) {
        if let Some(tcp) = conn.inner.get_ref().get_ref().inner.get_ref().get_tcpstream() {
            opt.apply(tcp)
        }
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

        let (mut sink, stream) = wsstream.split();

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                if sink.send(Message::Ping(vec![0x13, 0x37])).await.is_err() {
                    tracing::info!("Heartbeat failed, closing connection.");
                    break;
                }
            }
        });

        let tun = WebsocketTunnel {
            inner: StreamReader::new(StreamWrapper::new(stream)),
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

        let (mut sink, stream) = wsstream.split();

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                if sink.send(Message::Ping(vec![0x13, 0x37])).await.is_err() {
                    tracing::info!("Heartbeat failed, closing connection.");
                    break;
                }
            }
        });

        let tun = WebsocketTunnel {
            inner: StreamReader::new(StreamWrapper::new(stream)),
        };
        Ok(tun)
    }
}