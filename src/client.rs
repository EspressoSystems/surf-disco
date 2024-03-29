// Copyright (c) 2022 Espresso Systems (espressosys.com)
// This file is part of the surf-disco library.

// You should have received a copy of the MIT License
// along with the surf-disco library. If not, see <https://mit-license.org/>.

use crate::{http, Error, Method, Request, SocketRequest, StatusCode, Url};
use async_std::task::sleep;
use derivative::Derivative;
use serde::de::DeserializeOwned;
use std::time::{Duration, Instant};
use surf::http::headers::ACCEPT;
use versioned_binary_serialization::version::StaticVersionType;

pub use tide_disco::healthcheck::{HealthCheck, HealthStatus};

/// A client of a Tide Disco application.
#[derive(Derivative)]
#[derivative(Clone(bound = ""), Debug(bound = ""))]
pub struct Client<E, VER: StaticVersionType> {
    inner: surf::Client,
    _marker: std::marker::PhantomData<fn(E, VER) -> ()>,
}

impl<E: Error, VER: StaticVersionType> Default for Client<E, VER> {
    fn default() -> Self {
        Self {
            inner: surf::Config::new().try_into().unwrap(),
            _marker: Default::default(),
        }
    }
}

impl<E: Error, VER: StaticVersionType> Client<E, VER> {
    /// Create a client and connect to the Tide Disco server at `base_url`.
    pub fn new(base_url: Url) -> Self {
        Self::builder(base_url).build()
    }

    /// Create a client with customization.
    pub fn builder(base_url: Url) -> ClientBuilder<E, VER> {
        ClientBuilder::<E, VER>::new(base_url)
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
                _ => sleep(Duration::from_secs(10)).await,
            }
        }
        false
    }

    /// Connect to the server, retrying until the server is `healthy`.
    ///
    /// This function is similar to [connect](Self::connect). It will make requests to the
    /// `/healthcheck` endpoint until a request succeeds. However, it will then continue retrying
    /// until the response from `/healthcheck` satisfies the `healthy` predicate.
    ///
    /// On success, returns the response from `/healthcheck`. On timeout, returns `None`.
    pub async fn wait_for_health<H: DeserializeOwned + HealthCheck>(
        &self,
        healthy: impl Fn(&H) -> bool,
        timeout: Option<Duration>,
    ) -> Option<H> {
        let timeout = timeout.map(|d| Instant::now() + d);
        while timeout.map(|t| Instant::now() < t).unwrap_or(true) {
            match self.healthcheck::<H>().await {
                Ok(health) if healthy(&health) => return Some(health),
                _ => sleep(Duration::from_secs(10)).await,
            }
        }
        None
    }

    /// Build an HTTP `GET` request.
    pub fn get<T: DeserializeOwned>(&self, route: &str) -> Request<T, E, VER> {
        self.request(Method::Get, route)
    }

    /// Build an HTTP `POST` request.
    pub fn post<T: DeserializeOwned>(&self, route: &str) -> Request<T, E, VER> {
        self.request(Method::Post, route)
    }

    /// Query the server's healthcheck endpoint.
    pub async fn healthcheck<H: DeserializeOwned + HealthCheck>(&self) -> Result<H, E> {
        self.get("healthcheck").send().await
    }

    /// Build an HTTP request with the specified method.
    pub fn request<T: DeserializeOwned>(&self, method: Method, route: &str) -> Request<T, E, VER> {
        let req: Request<T, E, VER> = self.inner.request(method, route).into();
        // By default, request binary content from the server, as this is the most compact format
        // supported by all Tide Disco applications.
        req.header(ACCEPT, "application/octet-stream")
    }

    /// Build a streaming connection request.
    ///
    /// # Panics
    ///
    /// This will panic if a malformed URL is passed.
    pub fn socket(&self, route: &str) -> SocketRequest<E, VER> {
        self.inner
            .config()
            .base_url
            .as_ref()
            .unwrap()
            .join(route)
            .unwrap()
            .into()
    }

    /// Create a client for a sub-module of the connected application.
    pub fn module<ModError: Error>(
        &self,
        prefix: &str,
    ) -> Result<Client<ModError, VER>, http::url::ParseError> {
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

/// Interface to specify optional configuration values before creating a [Client].
pub struct ClientBuilder<E: Error, VER: StaticVersionType> {
    config: surf::Config,
    _marker: std::marker::PhantomData<fn(E, VER) -> ()>,
}

impl<E: Error, VER: StaticVersionType> ClientBuilder<E, VER> {
    fn new(mut base_url: Url) -> Self {
        // If the path part of `base_url` does not end in `/`, `join` will treat it as a filename
        // and remove it, which is never what we want: `base_url` is _always_ a directory-like path.
        // To avoid the annoyance of having every caller add a trailing slash if necessary, we will
        // add a trailing slash here if there isn't one already.
        if !base_url.path().ends_with('/') {
            base_url.set_path(&format!("{}/", base_url.path()));
        }
        Self {
            config: surf::Config::new().set_base_url(base_url),
            _marker: Default::default(),
        }
    }

    /// Set connection timeout duration.
    ///
    /// Passing `None` will remove the timeout.
    ///
    /// Default: `Some(Duration::from_secs(60))`.
    pub fn set_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.config = self.config.set_timeout(timeout);
        self
    }

    /// Create a [Client] with the settings specified in this builder.
    pub fn build(self) -> Client<E, VER> {
        // This `unwrap` can only fail if [surf] is built without the `default-client` feature flag,
        // which is a default feature and one we require to build this crate.
        let inner = self.config.try_into().unwrap();
        Client {
            inner,
            _marker: Default::default(),
        }
    }
}

impl<E: Error, VER: StaticVersionType> From<ClientBuilder<E, VER>> for Client<E, VER> {
    fn from(builder: ClientBuilder<E, VER>) -> Self {
        builder.build()
    }
}

#[cfg(test)]
mod test {
    use crate::socket::Connection;

    use super::*;
    use async_std::{sync::RwLock, task::spawn};
    use futures::{stream::iter, FutureExt, SinkExt, StreamExt};
    use portpicker::pick_unused_port;
    use serde::{Deserialize, Serialize};
    use tide_disco::{error::ServerError, App};
    use toml::toml;
    use versioned_binary_serialization::version::StaticVersion;
    type Ver01 = StaticVersion<0, 1>;
    const VER_0_1: Ver01 = StaticVersion {};

    #[async_std::test]
    async fn test_basic_http_client() {
        // Set up a simple Tide Disco app as an example.
        let mut app: App<(), ServerError, Ver01> = App::with_state(());
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
                    if req.body_auto::<String, _>(VER_0_1).unwrap() == "body" {
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
        spawn(app.serve(format!("0.0.0.0:{}", port), VER_0_1));

        // Connect a client.
        let client = Client::<ServerError, Ver01>::new(
            format!("http://localhost:{}", port).parse().unwrap(),
        );
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
        if err.status != StatusCode::BadRequest || err.message != "invalid body" {
            panic!("unexpected error {}", err);
        }
    }

    #[async_std::test]
    async fn test_streaming_client() {
        // Set up a simple Tide Disco app as an example.
        let mut app: App<(), ServerError, Ver01> = App::with_state(());
        let api = toml! {
            [route.echo]
            PATH = ["/echo"]
            METHOD = "SOCKET"

            [route.naturals]
            PATH = ["/naturals/:max"]
            METHOD = "SOCKET"
            ":max" = "Integer"
        };
        app.module::<ServerError>("mod", api)
            .unwrap()
            .socket::<_, String, String>("echo", |_req, mut conn, _state| {
                async move {
                    while let Some(Ok(msg)) = conn.next().await {
                        conn.send(&msg).await.unwrap();
                    }
                    Ok(())
                }
                .boxed()
            })
            .unwrap()
            .stream("naturals", |req, _state| {
                iter(0..req.integer_param("max").unwrap()).map(Ok).boxed()
            })
            .unwrap();
        let port = pick_unused_port().unwrap();
        spawn(app.serve(format!("0.0.0.0:{}", port), VER_0_1));

        // Connect a client.
        let client: Client<ServerError, _> =
            Client::new(format!("http://localhost:{}", port).parse().unwrap());
        assert!(client.connect(None).await);

        // Test a bidirectional endpoint.
        let mut conn: Connection<_, _, _, Ver01> = client
            .socket("mod/echo")
            .connect::<String, String>()
            .await
            .unwrap();
        conn.send(&"foo".into()).await.unwrap();
        assert_eq!(conn.next().await.unwrap().unwrap(), "foo");
        conn.send(&"bar".into()).await.unwrap();
        assert_eq!(conn.next().await.unwrap().unwrap(), "bar");

        // Test a streaming endpoint.
        assert_eq!(
            client
                .socket("mod/naturals/10")
                .subscribe::<u64>()
                .await
                .unwrap()
                .collect::<Vec<_>>()
                .await,
            (0..10).map(Ok).collect::<Vec<_>>()
        );
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
    enum HealthCheck {
        Ready,
        Initializing,
    }

    impl super::HealthCheck for HealthCheck {
        fn status(&self) -> StatusCode {
            StatusCode::Ok
        }
    }

    #[async_std::test]
    async fn test_healthcheck() {
        // Set up a simple Tide Disco app as an example.
        let mut app: App<_, ServerError, Ver01> =
            App::with_state(RwLock::new(HealthCheck::Initializing));
        let api = toml! {
            [route.init]
            PATH = ["/init"]
            METHOD = "POST"
        };
        app.module::<ServerError>("mod", api)
            .unwrap()
            .with_health_check(|state| async move { *state.read().await }.boxed())
            .post("init", |_, state| {
                async move {
                    *state = HealthCheck::Ready;
                    Ok(())
                }
                .boxed()
            })
            .unwrap();
        let port = pick_unused_port().unwrap();
        spawn(app.serve(format!("0.0.0.0:{}", port), VER_0_1));

        // Connect a client.
        let client = Client::<ServerError, Ver01>::new(
            format!("http://localhost:{}/mod", port).parse().unwrap(),
        );
        assert!(client.connect(None).await);
        assert_eq!(
            HealthCheck::Initializing,
            client.healthcheck().await.unwrap()
        );

        // Waiting for [HealthCheck::Ready] should time out.
        assert_eq!(
            client
                .wait_for_health::<HealthCheck>(
                    |h| *h == HealthCheck::Ready,
                    Some(Duration::from_secs(1))
                )
                .await,
            None
        );

        // Initialize the service.
        client.post::<()>("init").send().await.unwrap();

        // Now waiting for [HealthCheck::Ready] should succeed.
        assert_eq!(
            client
                .wait_for_health::<HealthCheck>(|h| *h == HealthCheck::Ready, None)
                .await,
            Some(HealthCheck::Ready)
        );
        assert_eq!(HealthCheck::Ready, client.healthcheck().await.unwrap());
    }

    #[test]
    fn test_builder() {
        let client =
            Client::<ServerError, Ver01>::builder("http://www.example.com".parse().unwrap())
                .set_timeout(None)
                .build();
        assert_eq!(
            client.inner.config().base_url,
            Some("http://www.example.com".parse().unwrap())
        );
        assert_eq!(client.inner.config().http_config.timeout, None);
    }
}
