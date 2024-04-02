// Copyright (c) 2022 Espresso Systems (espressosys.com)
// This file is part of the surf-disco library.

// You should have received a copy of the MIT License
// along with the surf-disco library. If not, see <https://mit-license.org/>.

use crate::{
    http::headers::{HeaderName, ToHeaderValues},
    Error, StatusCode, Url,
};
use async_tungstenite::{
    async_std::{connect_async, ConnectStream},
    tungstenite::{http::request::Builder as RequestBuilder, Error as WsError, Message},
    WebSocketStream,
};
use futures::{
    task::{Context, Poll},
    Sink, Stream,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{collections::HashMap, pin::Pin};
use vbs::{version::StaticVersionType, BinarySerializer, Serializer};

#[must_use]
#[derive(Debug)]
pub struct SocketRequest<E, VER: StaticVersionType> {
    url: Url,
    headers: HashMap<String, Vec<String>>,
    marker: std::marker::PhantomData<fn(E, VER) -> ()>,
}

impl<E, VER: StaticVersionType> From<Url> for SocketRequest<E, VER> {
    fn from(mut url: Url) -> Self {
        url.set_scheme(&socket_scheme(url.scheme())).unwrap();
        Self {
            url,
            headers: Default::default(),
            marker: Default::default(),
        }
    }
}

impl<E: Error, VER: StaticVersionType> SocketRequest<E, VER> {
    /// Set a header on the request.
    pub fn header(mut self, key: impl Into<HeaderName>, values: impl ToHeaderValues) -> Self {
        let name = key.into().to_string();
        for value in values.to_header_values().unwrap() {
            self.headers
                .entry(name.clone())
                .or_default()
                .push(value.to_string());
        }
        self
    }

    /// Start the WebSocket handshake and initiate a connection to the server.
    pub async fn connect<FromServer: DeserializeOwned, ToServer: Serialize + ?Sized>(
        mut self,
    ) -> Result<Connection<FromServer, ToServer, E, VER>, E> {
        // Follow redirects.
        loop {
            let mut req = RequestBuilder::new().uri(self.url.to_string());
            for (key, values) in &self.headers {
                for value in values {
                    req = req.header(key, value);
                }
            }
            let req = req
                .body(())
                .map_err(|err| E::catch_all(StatusCode::BadRequest, err.to_string()))?;

            let err = match connect_async(req).await {
                Ok((conn, _)) => return Ok(conn.into()),
                Err(err) => err,
            };
            if let WsError::Http(res) = &err {
                if (301..=308).contains(&u16::from(res.status())) {
                    if let Some(location) = res
                        .headers()
                        .get("location")
                        .and_then(|header| header.to_str().ok())
                    {
                        tracing::info!(from = %self.url, to = %location, "WS handshake following redirect");
                        self.url.set_path(location);
                        continue;
                    }
                }
            }
            return Err(E::catch_all(StatusCode::BadRequest, err.to_string()));
        }
    }

    /// Initiate a unidirectional connection to the server.
    ///
    /// This is equivalent to `self.connect()` with the `ToServer` message type replaced by
    /// [Unsupported], so that you don't have to specify the type parameter if it isn't used.
    pub async fn subscribe<FromServer: DeserializeOwned>(
        self,
    ) -> Result<Connection<FromServer, Unsupported, E, VER>, E> {
        self.connect().await
    }
}

/// A bi-directional connection to a WebSocket server.
pub struct Connection<FromServer, ToServer: ?Sized, E, VER: StaticVersionType> {
    inner: WebSocketStream<ConnectStream>,
    #[allow(clippy::type_complexity)]
    marker: std::marker::PhantomData<fn(FromServer, ToServer, E, VER) -> ()>,
}

impl<FromServer, ToServer: ?Sized, E, VER: StaticVersionType> From<WebSocketStream<ConnectStream>>
    for Connection<FromServer, ToServer, E, VER>
{
    fn from(inner: WebSocketStream<ConnectStream>) -> Self {
        Self {
            inner,
            marker: Default::default(),
        }
    }
}

impl<FromServer: DeserializeOwned, ToServer: ?Sized, E: Error, VER: StaticVersionType> Stream
    for Connection<FromServer, ToServer, E, VER>
{
    type Item = Result<FromServer, E>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Get a `Pin<&mut WebSocketStream>` for the underlying connection, so we can use the
        // `Stream` implementation of that field.
        match self.pinned_inner().poll_next(cx) {
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(Err(err))) => match err {
                WsError::ConnectionClosed | WsError::AlreadyClosed => Poll::Ready(None),
                err => Poll::Ready(Some(Err(E::catch_all(
                    StatusCode::InternalServerError,
                    err.to_string(),
                )))),
            },
            Poll::Ready(Some(Ok(msg))) => Poll::Ready(match msg {
                Message::Binary(bytes) => {
                    Some(Serializer::<VER>::deserialize(&bytes).map_err(|err| {
                        E::catch_all(
                            StatusCode::InternalServerError,
                            format!("invalid binary serialization: {}", err),
                        )
                    }))
                }
                Message::Text(s) => Some(serde_json::from_str(&s).map_err(|err| {
                    E::catch_all(
                        StatusCode::InternalServerError,
                        format!("invalid JSON: {}", err),
                    )
                })),
                Message::Close(_) => None,
                _ => Some(Err(E::catch_all(
                    StatusCode::UnsupportedMediaType,
                    "unsupported WebSocket message".into(),
                ))),
            }),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<FromServer, ToServer: Serialize + ?Sized, E: Error, VER: StaticVersionType> Sink<&ToServer>
    for Connection<FromServer, ToServer, E, VER>
{
    type Error = E;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.pinned_inner().poll_ready(cx).map_err(|err| {
            E::catch_all(
                StatusCode::InternalServerError,
                format!("error in WebSocket connection: {}", err),
            )
        })
    }

    fn start_send(self: Pin<&mut Self>, item: &ToServer) -> Result<(), Self::Error> {
        let msg = Message::Binary(Serializer::<VER>::serialize(item).map_err(|err| {
            E::catch_all(
                StatusCode::BadRequest,
                format!("invalid binary serialization: {}", err),
            )
        })?);
        self.pinned_inner().start_send(msg).map_err(|err| {
            E::catch_all(
                StatusCode::InternalServerError,
                format!("error sending WebSocket message: {}", err),
            )
        })
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.pinned_inner().poll_flush(cx).map_err(|err| {
            E::catch_all(
                StatusCode::InternalServerError,
                format!("error in WebSocket connection: {}", err),
            )
        })
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.pinned_inner().poll_close(cx).map_err(|err| {
            E::catch_all(
                StatusCode::InternalServerError,
                format!("error in WebSocket connection: {}", err),
            )
        })
    }
}

impl<FromServer, ToServer: ?Sized, E, VER: StaticVersionType>
    Connection<FromServer, ToServer, E, VER>
{
    /// Project a `Pin<&mut Self>` to a pinned reference to the underlying connection.
    fn pinned_inner(self: Pin<&mut Self>) -> Pin<&mut WebSocketStream<ConnectStream>> {
        // # Soundness
        //
        // This implements _structural pinning_ for [Connection]. This comes with some requirements
        // to maintain safety, as described at
        // https://doc.rust-lang.org/std/pin/index.html#pinning-is-structural-for-field:
        //
        // 1. The struct must only be [Unpin] if all the structural fields are [Unpin]. This is the
        //    default, and we don't explicitly implement [Unpin] for [Connection].
        // 2. The destructor of the struct must not move structural fields out of its argument. This
        //    is enforced by the compiler in our [Drop] implementation, which follows the idiom for
        //    safe [Drop] implementations for pinned structs.
        // 3. You must make sure that you uphold the [Drop] guarantee: once your struct is pinned,
        //    the memory that contains the content is not overwritten or deallocated without calling
        //    the content’s destructors. This is also enforced by our [Drop] implementation.
        // 4. You must not offer any other operations that could lead to data being moved out of the
        //    structural fields when your type is pinned. There are no operations on this type that
        //    move out of `inner`.
        unsafe { self.map_unchecked_mut(|s| &mut s.inner) }
    }
}

impl<FromServer, ToServer: ?Sized, E, VER: StaticVersionType> Drop
    for Connection<FromServer, ToServer, E, VER>
{
    fn drop(&mut self) {
        // This is the idiomatic way to implement [drop] for a type that uses pinning. Since [drop]
        // is implicitly called with `&mut self` even on types that were pinned, we place any
        // implementation inside [inner_drop], which takes `Pin<&mut Self>`, when the commpiler will
        // be able to check that we do not do anything that we couldn't have done on a
        // `Pin<&mut Self>`.
        //
        // The [drop] implementation for this type is trivial, and it would be safe to use the
        // automatically generated [drop] implementation, but we nonetheless implement [drop]
        // explicitly in the idiomatic fashion so that it is impossible to accidentally implement an
        // unsafe version of [drop] for this type in the future.

        // `new_unchecked` is okay because we know this value is never used again after being
        // dropped.
        inner_drop(unsafe { Pin::new_unchecked(self) });
        fn inner_drop<FromServer, ToServer: ?Sized, E, VER: StaticVersionType>(
            _this: Pin<&mut Connection<FromServer, ToServer, E, VER>>,
        ) {
            // Any logic goes here.
        }
    }
}

/// Unconstructable enum used to disable the [Sink] functionality of [Connection].
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum Unsupported {}

/// Get the scheme for a WebSockets connection upgraded from an existing stateless connection.
///
/// `scheme` is the scheme of the stateless connection, e.g. HTTP or HTTPS. If the scheme has a
/// known WebSockets counterpart, e.g. WS or WSS, we return it. Otherwise we trust the user knows
/// what they're doing and return `scheme` unmodified.
fn socket_scheme(scheme: &str) -> String {
    match scheme {
        "http" => "ws",
        "https" => "wss",
        _ => scheme,
    }
    .to_string()
}
