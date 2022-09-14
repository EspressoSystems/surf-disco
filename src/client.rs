use crate::{http, Error, Method, Request, StatusCode, Url};
use serde::de::DeserializeOwned;
use std::time::{Duration, Instant};
use surf::http::headers::ACCEPT;

/// A client of a Tide Disco application.
#[derive(Clone, Debug)]
pub struct Client<E> {
    inner: surf::Client,
    _marker: std::marker::PhantomData<fn(E) -> ()>,
}

impl<E: Error> Client<E> {
    /// Create a client and connect to the Tide Disco server at `base_url`.
    pub fn new(base_url: Url) -> Self {
        // This `unwrap` can only fail if [surf] is built without the `default-client` feature flag,
        // which is a default feature and one we require to build this crate.
        let inner = surf::Config::new()
            .set_base_url(base_url)
            .try_into()
            .unwrap();
        Self {
            inner,
            _marker: Default::default(),
        }
    }

    /// Connect to the server, retrying if the server is not running.
    ///
    /// It is not necessary to call this function when creating a new client. The client will
    /// automatically connect when a request is made, if the server is available. However, this can
    /// be useful to wait for the server to come up, if the server may be offline when the client is
    /// created.
    ///
    /// This function will make an HTTP `GET` request to the server's `/healthcheck` endpoint, to
    /// test if the server is available. If this request succeeds, [connect](Self::connect) returns
    /// `true`. Otherwise, the client will continue retrying `/healthcheck` requests until `timeout`
    /// has elapsed (or forever, if `timeout` is `None`). If the timeout expires before a
    /// `/healthcheck` request succeeds, [connect](Self::connect) will return `false`.
    pub async fn connect(&self, timeout: Option<Duration>) -> bool {
        let timeout = timeout.map(|d| Instant::now() + d);
        while timeout.map(|t| Instant::now() < t).unwrap_or(true) {
            match self.inner.get("/healthcheck").send().await {
                Ok(res) if res.status() == StatusCode::Ok => return true,
                _ => continue,
            }
        }
        false
    }

    /// Build an HTTP `GET` request.
    pub fn get<T: DeserializeOwned>(&self, route: &str) -> Request<T, E> {
        self.request(Method::Get, route)
    }

    /// Build an HTTP `POST` request.
    pub fn post<T: DeserializeOwned>(&self, route: &str) -> Request<T, E> {
        self.request(Method::Post, route)
    }

    /// Build an HTTP request with the specified method.
    pub fn request<T: DeserializeOwned>(&self, method: Method, route: &str) -> Request<T, E> {
        let req: Request<T, E> = self.inner.request(method, route).into();
        // By default, request binary content from the server, as this is the most compact format
        // supported by all Tide Disco applications.
        req.header(ACCEPT, "application/octet-stream")
    }

    /// Create a client for a sub-module of the connected application.
    pub fn module<ModError: Error>(
        &self,
        prefix: &str,
    ) -> Result<Client<ModError>, http::url::ParseError> {
        Ok(Client::new(
            self.inner
                .config()
                .base_url
                .as_ref()
                .unwrap()
                .join(prefix)?,
        ))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use async_std::task::spawn;
    use futures::FutureExt;
    use portpicker::pick_unused_port;
    use tide_disco::{error::ServerError, App};
    use toml::toml;

    #[async_std::test]
    async fn test_basic_http_client() {
        // Set up a simple Tide Disco app as an example.
        let mut app: App<(), ServerError> = App::with_state(());
        let api = toml! {
            [route.get]
            PATH = ["/get"]
            METHOD = "GET"

            [route.post]
            PATH = ["/post"]
            METHOD = "POST"
        };
        app.module::<ServerError>("mod", api)
            .unwrap()
            .get("get", |_req, _state| async move { Ok("response") }.boxed())
            .unwrap()
            .post("post", |req, _state| {
                async move {
                    if req.body_auto::<String>().unwrap() == "body" {
                        Ok("response")
                    } else {
                        Err(ServerError::catch_all(
                            StatusCode::BadRequest,
                            "invalid body".into(),
                        ))
                    }
                }
                .boxed()
            })
            .unwrap();
        let port = pick_unused_port().unwrap();
        spawn(app.serve(format!("0.0.0.0:{}", port)));

        // Connect a client.
        let client =
            Client::<ServerError>::new(format!("http://localhost:{}", port).parse().unwrap());
        assert!(client.connect(None).await);

        // Test a couple of basic requests.
        assert_eq!(
            client.get::<String>("mod/get").send().await.unwrap(),
            "response"
        );
        assert_eq!(
            client
                .post::<String>("mod/post")
                .body_json(&"body".to_string())
                .unwrap()
                .send()
                .await
                .unwrap(),
            "response"
        );

        // Test an error response.
        let err = client
            .post::<String>("mod/post")
            .body_json(&"bad".to_string())
            .unwrap()
            .send()
            .await
            .unwrap_err();
        println!(
            "{}",
            hex::encode(
                &bincode::serialize(&ServerError::catch_all(
                    StatusCode::BadRequest,
                    "invalid body".into()
                ))
                .unwrap()
            )
        );
        if err.status != StatusCode::BadRequest || err.message != "invalid body" {
            panic!("unexpected error {}", err);
        }
    }
}
