use std::io::Write;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::{io, time};

use actix_codec::{AsyncRead, AsyncWrite, Framed, ReadBuf};
use bytes::buf::BufMut;
use bytes::{Bytes, BytesMut};
use futures_core::Stream;
use futures_util::{future::poll_fn, SinkExt as _};

use crate::error::PayloadError;
use crate::h1;
use crate::header::HeaderMap;
use crate::http::{
    header::{IntoHeaderValue, EXPECT, HOST},
    StatusCode,
};
use crate::message::{RequestHeadType, ResponseHead};
use crate::payload::{Payload, PayloadStream};

use super::connection::ConnectionType;
use super::error::{ConnectError, SendRequestError};
use super::pool::Acquired;
use crate::body::{BodySize, MessageBody};

pub(crate) async fn send_request<T, B>(
    io: T,
    mut head: RequestHeadType,
    body: B,
    created: time::Instant,
    acquired: Acquired<T>,
) -> Result<(ResponseHead, Payload), SendRequestError>
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
    B: MessageBody,
{
    // set request host header
    if !head.as_ref().headers.contains_key(HOST)
        && !head.extra_headers().iter().any(|h| h.contains_key(HOST))
    {
        if let Some(host) = head.as_ref().uri.host() {
            let mut wrt = BytesMut::with_capacity(host.len() + 5).writer();

            match head.as_ref().uri.port_u16() {
                None | Some(80) | Some(443) => write!(wrt, "{}", host)?,
                Some(port) => write!(wrt, "{}:{}", host, port)?,
            };

            match wrt.get_mut().split().freeze().try_into_value() {
                Ok(value) => match head {
                    RequestHeadType::Owned(ref mut head) => {
                        head.headers.insert(HOST, value);
                    }
                    RequestHeadType::Rc(_, ref mut extra_headers) => {
                        let headers = extra_headers.get_or_insert(HeaderMap::new());
                        headers.insert(HOST, value);
                    }
                },
                Err(e) => log::error!("Can not set HOST header {}", e),
            }
        }
    }

    let io = H1Connection {
        created,
        acquired,
        io: Some(io),
    };

    // create Framed and prepare sending request
    let mut framed = Framed::new(io, h1::ClientCodec::default());

    // Check EXPECT header and enable expect handle flag accordingly.
    //
    // RFC: https://tools.ietf.org/html/rfc7231#section-5.1.1
    let is_expect = if head.as_ref().headers.contains_key(EXPECT) {
        match body.size() {
            BodySize::None | BodySize::Empty | BodySize::Sized(0) => {
                let keep_alive = framed.codec_ref().keepalive();
                framed.io_mut().on_release(keep_alive);

                // TODO: use a new variant or a new type better describing error violate
                // `Requirements for clients` session of above RFC
                return Err(SendRequestError::Connect(ConnectError::Disconnected));
            }
            _ => true,
        }
    } else {
        false
    };

    framed.send((head, body.size()).into()).await?;

    let mut pin_framed = Pin::new(&mut framed);

    // special handle for EXPECT request.
    let (do_send, mut res_head) = if is_expect {
        let head = poll_fn(|cx| pin_framed.as_mut().poll_next(cx))
            .await
            .ok_or(ConnectError::Disconnected)??;

        // return response head in case status code is not continue
        // and current head would be used as final response head.
        (head.status == StatusCode::CONTINUE, Some(head))
    } else {
        (true, None)
    };

    if do_send {
        // send request body
        match body.size() {
            BodySize::None | BodySize::Empty | BodySize::Sized(0) => {}
            _ => send_body(body, pin_framed.as_mut()).await?,
        };

        // read response and init read body
        let head = poll_fn(|cx| pin_framed.as_mut().poll_next(cx))
            .await
            .ok_or(ConnectError::Disconnected)??;

        res_head = Some(head);
    }

    let head = res_head.unwrap();

    match pin_framed.codec_ref().message_type() {
        h1::MessageType::None => {
            let keep_alive = pin_framed.codec_ref().keepalive();
            pin_framed.io_mut().on_release(keep_alive);

            Ok((head, Payload::None))
        }
        _ => {
            let pl: PayloadStream = Box::pin(PlStream::new(framed));
            Ok((head, pl.into()))
        }
    }
}

pub(crate) async fn open_tunnel<T>(
    io: T,
    head: RequestHeadType,
) -> Result<(ResponseHead, Framed<T, h1::ClientCodec>), SendRequestError>
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
{
    // create Framed and send request
    let mut framed = Framed::new(io, h1::ClientCodec::default());
    framed.send((head, BodySize::None).into()).await?;

    // read response
    let head = poll_fn(|cx| Pin::new(&mut framed).poll_next(cx))
        .await
        .ok_or(ConnectError::Disconnected)??;

    Ok((head, framed))
}

/// send request body to the peer
pub(crate) async fn send_body<T, B>(
    body: B,
    mut framed: Pin<&mut Framed<T, h1::ClientCodec>>,
) -> Result<(), SendRequestError>
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
    B: MessageBody,
{
    actix_rt::pin!(body);

    let mut eof = false;
    while !eof {
        while !eof && !framed.as_ref().is_write_buf_full() {
            match poll_fn(|cx| body.as_mut().poll_next(cx)).await {
                Some(result) => {
                    framed.as_mut().write(h1::Message::Chunk(Some(result?)))?;
                }
                None => {
                    eof = true;
                    framed.as_mut().write(h1::Message::Chunk(None))?;
                }
            }
        }

        if !framed.as_ref().is_write_buf_empty() {
            poll_fn(|cx| match framed.as_mut().flush(cx) {
                Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
                Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
                Poll::Pending => {
                    if !framed.as_ref().is_write_buf_full() {
                        Poll::Ready(Ok(()))
                    } else {
                        Poll::Pending
                    }
                }
            })
            .await?;
        }
    }

    framed.get_mut().flush().await?;
    Ok(())
}

#[doc(hidden)]
/// HTTP client connection
pub struct H1Connection<T>
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
{
    /// T should be `Unpin`
    io: Option<T>,
    created: time::Instant,
    acquired: Acquired<T>,
}

impl<T> H1Connection<T>
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
{
    fn on_release(&mut self, keep_alive: bool) {
        if keep_alive {
            self.release();
        } else {
            self.close();
        }
    }

    /// Close connection
    fn close(&mut self) {
        if let Some(io) = self.io.take() {
            self.acquired.close(ConnectionType::H1(io));
        }
    }

    /// Release this connection to the connection pool
    fn release(&mut self) {
        if let Some(io) = self.io.take() {
            self.acquired.release(ConnectionType::H1(io), self.created);
        }
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin + 'static> AsyncRead for H1Connection<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io.as_mut().unwrap()).poll_read(cx, buf)
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin + 'static> AsyncWrite for H1Connection<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io.as_mut().unwrap()).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(self.io.as_mut().unwrap()).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Pin::new(self.io.as_mut().unwrap()).poll_shutdown(cx)
    }
}

#[pin_project::pin_project]
pub(crate) struct PlStream<Io>
where
    Io: AsyncRead + AsyncWrite + Unpin + 'static,
{
    #[pin]
    framed: Option<Framed<H1Connection<Io>, h1::ClientPayloadCodec>>,
}

impl<Io> PlStream<Io>
where
    Io: AsyncRead + AsyncWrite + Unpin + 'static,
{
    fn new(framed: Framed<H1Connection<Io>, h1::ClientCodec>) -> Self {
        let framed = framed.into_map_codec(|codec| codec.into_payload_codec());

        PlStream {
            framed: Some(framed),
        }
    }
}

impl<Io> Stream for PlStream<Io>
where
    Io: AsyncRead + AsyncWrite + Unpin + 'static,
{
    type Item = Result<Bytes, PayloadError>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let mut framed = self.project().framed.as_pin_mut().unwrap();

        match framed.as_mut().next_item(cx)? {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(chunk)) => {
                if let Some(chunk) = chunk {
                    Poll::Ready(Some(Ok(chunk)))
                } else {
                    let keep_alive = framed.codec_ref().keepalive();
                    framed.io_mut().on_release(keep_alive);
                    Poll::Ready(None)
                }
            }
            Poll::Ready(None) => Poll::Ready(None),
        }
    }
}
