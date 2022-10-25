use crate::{Error, StatusCode};
use serde::{de::DeserializeOwned, Serialize};
use std::fmt::Display;
use surf::http::headers::{HeaderName, ToHeaderValues};

#[must_use]
#[derive(Debug)]
pub struct Request<T, E> {
    inner: surf::RequestBuilder,
    marker: std::marker::PhantomData<fn(T, E) -> ()>,
}

impl<T, E> From<surf::RequestBuilder> for Request<T, E> {
    fn from(inner: surf::RequestBuilder) -> Self {
        Self {
            inner,
            marker: Default::default(),
        }
    }
}

impl<T: DeserializeOwned, E: Error> Request<T, E> {
    /// Set a header on the request.
    pub fn header(self, key: impl Into<HeaderName>, value: impl ToHeaderValues) -> Self {
        self.inner.header(key, value).into()
    }

    /// Set the request body using JSON.
    ///
    /// Body is serialized using [serde_json] and the `Content-Type` header is set to
    /// `application/json`.
    ///
    /// # Errors
    ///
    /// Fails if `body` does not serialize successfully.
    pub fn body_json<B: Serialize>(self, body: &B) -> Result<Self, E> {
        self.inner
            .body_json(body)
            .map(Self::from)
            .map_err(surf_error)
    }

    /// Set the request body using [bincode].
    ///
    /// Body is serialized using [bincode] and the `Content-Type` header is set to
    /// `application/octet-stream`.
    ///
    /// # Errors
    ///
    /// Fails if `body` does not serialize successfully.
    pub fn body_binary<B: Serialize>(self, body: &B) -> Result<Self, E> {
        Ok(self
            .inner
            .body_bytes(&bincode::serialize(body).map_err(request_error)?)
            .into())
    }

    /// Send the request and await a response from the server.
    ///
    /// If the request succeeds (receives a response with [StatusCode::Ok]) the response body is
    /// converted to a `T`, using a format determined by the `Content-Type` header of the request.
    ///
    /// # Errors
    ///
    /// If the client is unable to reach the server, or if the response body cannot be interpreted
    /// as a `T`, an error message is created using [catch_all](Error::catch_all) and returned.
    ///
    /// If the request completes but the response status code is not [StatusCode::Ok], an error
    /// message is constructed using the body of the response. If there is a body and it can be
    /// converted to an `E` using the content type specified in the response's `Content-Type`
    /// header, that `E` will be returned directly. Otherwise, an error message is synthesized using
    /// [catch_all](Error::catch_all) that includes human-readable information about the response.
    pub async fn send(self) -> Result<T, E> {
        let mut res = self.inner.send().await.map_err(surf_error)?;
        if res.status() == StatusCode::Ok {
            // If the response indicates success, deserialize the body using a format determined by
            // the Content-Type header.
            if let Some(content_type) = res.header("Content-Type").cloned() {
                match content_type.as_str() {
                    "application/json" => res.body_json().await.map_err(surf_error),
                    "application/octet-stream" => {
                        bincode::deserialize(&res.body_bytes().await.map_err(surf_error)?)
                            .map_err(request_error)
                    }
                    content_type => {
                        // For help in debugging, include the body with the unexpected content type
                        // in the error message.
                        let msg = match res.body_bytes().await {
                            Ok(bytes) => match std::str::from_utf8(&bytes) {
                                Ok(s) => format!("body: {}", s),
                                Err(_) => format!("body: {}", hex::encode(&bytes)),
                            },
                            Err(_) => String::default(),
                        };
                        Err(E::catch_all(
                            StatusCode::UnsupportedMediaType,
                            format!("unsupported content type {} {}", content_type, msg),
                        ))
                    }
                }
            } else {
                Err(E::catch_all(
                    StatusCode::UnsupportedMediaType,
                    "unspecified content type in response".into(),
                ))
            }
        } else {
            // To add context to the error, try to interpret the response body as a serialized
            // error. Since `body_json`, `body_string`, etc. consume the response body, we will
            // extract the body as raw bytes and then try various potential decodings based on the
            // response headers and the contents of the body.
            let bytes = match res.body_bytes().await {
                Ok(bytes) => bytes,
                Err(err) => {
                    // If we are unable to even read the body, just return a generic error message
                    // based on the status code.
                    return Err(E::catch_all(
                        res.status(),
                        format!(
                            "Request terminated with error {}. Failed to read request body due to {}",
                            res.status(),
                            err
                        ),
                    ));
                }
            };
            if let Some(content_type) = res.header("Content-Type") {
                // If the response specifies a content type, check if it is one of the types we know
                // how to deserialize, and if it is, we can then see if it deserializes to an `E`.
                match content_type.as_str() {
                    "application/json" => {
                        if let Ok(err) = serde_json::from_slice(&bytes) {
                            return Err(err);
                        }
                    }
                    "application/octet-stream" => {
                        if let Ok(err) = bincode::deserialize(&bytes) {
                            return Err(err);
                        }
                    }
                    _ => {}
                }
            }
            // If we get here, then we were not able to interpret the response body as an `E`
            // directly. This can be because:
            //  * the content type is not supported for deserialization
            //  * the content type was unspecified
            //  * the body did not deserialize to an `E` We have one thing left we can try: if the
            //    body is a string, we can use the `catch_all` variant of `E` to include the
            //    contents of the string in the error message.
            if let Ok(msg) = std::str::from_utf8(&bytes) {
                return Err(E::catch_all(res.status(), msg.to_string()));
            }

            // The response body was not an `E` or a string. Return the most helpful error message
            // we can, including the status code, content type, and raw body.
            Err(E::catch_all(
                res.status(),
                format!(
                    "Request terminated with error {}. Content-Type: {}. Body: 0x{}",
                    res.status(),
                    match res.header("Content-Type") {
                        Some(content_type) => content_type.as_str(),
                        None => "unspecified",
                    },
                    hex::encode(&bytes)
                ),
            ))
        }
    }
}

fn request_error<E: Error>(source: impl Display) -> E {
    E::catch_all(StatusCode::BadRequest, source.to_string())
}

fn surf_error<E: Error>(source: surf::Error) -> E {
    E::catch_all(source.status(), source.to_string())
}
