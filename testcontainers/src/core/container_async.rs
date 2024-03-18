use crate::{
    core::{env, env::Command, logs::LogStreamAsync, ports::Ports, WaitFor},
    Image, RunnableImage,
};
use async_trait::async_trait;
use bollard::models::{ContainerInspectResponse, HealthStatusEnum};
use futures::executor::block_on;
use std::{fmt, net::IpAddr, str::FromStr, time::Duration};
use tokio::time::sleep;

/// Represents a running docker container that has been started using an async client..
///
/// Containers have a [`custom destructor`][drop_impl] that removes them as soon as they
/// go out of scope. However, async drop is not available in rust yet. This implementation
/// is using block_on. Therefore required #[tokio::test(flavor = "multi_thread")] in your test
/// to use drop effectively. Otherwise your test might stall:
///
/// ```rust
/// use testcontainers::*;
/// #[tokio::test(flavor = "multi_thread")]
/// async fn a_test() {
///     let docker = clients::Http::default();
///
///     {
///         let container = docker.run(MyImage::default()).await;
///
///         // Docker container is stopped/removed at the end of this scope.
///     }
/// }
///
/// ```
///
/// [drop_impl]: struct.ContainerAsync.html#impl-Drop
pub struct ContainerAsync<I: Image> {
    id: String,
    docker_client: Box<dyn DockerAsync>,
    image: RunnableImage<I>,
    command: Command,
}

impl<I> ContainerAsync<I>
where
    I: Image,
{
    /// Returns the id of this container.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the mapped host port for an internal port of this docker container, on the host's
    /// IPv4 interfaces.
    ///
    /// This method does **not** magically expose the given port, it simply performs a mapping on
    /// the already exposed ports. If a docker container does not expose a port, this method will panic.
    ///
    /// # Panics
    ///
    /// This method panics if the given port is not mapped.
    /// Testcontainers is designed to be used in tests only. If a certain port is not mapped, the container
    /// is unlikely to be useful.
    #[deprecated(
        since = "0.13.1",
        note = "Use `get_host_port_ipv4()` or `get_host_port_ipv6()` instead."
    )]
    pub async fn get_host_port(&self, internal_port: u16) -> u16 {
        self.get_host_port_ipv4(internal_port).await
    }

    /// Returns the mapped host port for an internal port of this docker container, on the host's
    /// IPv4 interfaces.
    ///
    /// This method does **not** magically expose the given port, it simply performs a mapping on
    /// the already exposed ports. If a docker container does not expose a port, this method will panic.
    ///
    /// # Panics
    ///
    /// This method panics if the given port is not mapped.
    /// Testcontainers is designed to be used in tests only. If a certain port is not mapped, the container
    /// is unlikely to be useful.
    pub async fn get_host_port_ipv4(&self, internal_port: u16) -> u16 {
        self.docker_client
            .ports(&self.id)
            .await
            .map_to_host_port_ipv4(internal_port)
            .unwrap_or_else(|| {
                panic!(
                    "container {} does not expose port {}",
                    self.id, internal_port
                )
            })
    }

    /// Returns the mapped host port for an internal port of this docker container, on the host's
    /// IPv6 interfaces.
    ///
    /// This method does **not** magically expose the given port, it simply performs a mapping on
    /// the already exposed ports. If a docker container does not expose a port, this method will panic.
    ///
    /// # Panics
    ///
    /// This method panics if the given port is not mapped.
    /// Testcontainers is designed to be used in tests only. If a certain port is not mapped, the container
    /// is unlikely to be useful.
    pub async fn get_host_port_ipv6(&self, internal_port: u16) -> u16 {
        self.docker_client
            .ports(&self.id)
            .await
            .map_to_host_port_ipv6(internal_port)
            .unwrap_or_else(|| {
                panic!(
                    "container {} does not expose port {}",
                    self.id, internal_port
                )
            })
    }

    /// Returns the bridge ip address of docker container as specified in NetworkSettings.Networks.IPAddress
    pub async fn get_bridge_ip_address(&self) -> IpAddr {
        let result = self.docker_client.inspect(&self.id).await;

        let settings = result
            .network_settings
            .unwrap_or_else(|| panic!("container {} has no network settings", self.id));

        let mut networks = settings
            .networks
            .unwrap_or_else(|| panic!("container {} has no any networks", self.id));

        let bridge_name = self
            .image
            .network()
            .clone()
            .or(settings.bridge)
            .unwrap_or_else(|| panic!("container {} has missing bridge name", self.id));

        let ip = networks
            .remove(&bridge_name)
            .and_then(|network| network.ip_address)
            .unwrap_or_else(|| panic!("container {} has missing bridge IP", self.id));

        IpAddr::from_str(&ip)
            .unwrap_or_else(|_| panic!("container {} has invalid bridge IP", self.id))
    }

    pub async fn start(&self) {
        self.docker_client.start(&self.id).await
    }

    pub async fn stop(&self) {
        log::debug!("Stopping docker container {}", self.id);

        self.docker_client.stop(&self.id).await
    }

    pub async fn rm(self) {
        log::debug!("Deleting docker container {}", self.id);

        self.docker_client.rm(&self.id).await
    }

    async fn drop_async(&self) {
        match self.command {
            env::Command::Remove => self.docker_client.rm(&self.id).await,
            env::Command::Keep => {}
        }
        #[cfg(feature = "watchdog")]
        crate::watchdog::unregister(self.id());
    }
}

impl<I> fmt::Debug for ContainerAsync<I>
where
    I: fmt::Debug + Image,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContainerAsync")
            .field("id", &self.id)
            .field("image", &self.image)
            .finish()
    }
}

/// Represents Docker operations as an async trait.
///
/// This trait is `pub(crate)` to make sure we can make changes to this API without breaking clients.
/// Users should interact through the [`ContainerAsync`] API.
#[async_trait]
pub(crate) trait DockerAsync
where
    Self: Sync + Send,
{
    fn stdout_logs(&self, id: &str) -> LogStreamAsync<'_>;
    fn stderr_logs(&self, id: &str) -> LogStreamAsync<'_>;
    async fn ports(&self, id: &str) -> Ports;
    async fn inspect(&self, id: &str) -> ContainerInspectResponse;
    async fn rm(&self, id: &str);
    async fn stop(&self, id: &str);
    async fn start(&self, id: &str);
}

impl<I> ContainerAsync<I>
where
    I: Image,
{
    /// Constructs a new container given an id, a docker client and the image.
    /// ContainerAsync::new().await
    pub(crate) async fn new(
        id: String,
        docker_client: impl DockerAsync + 'static,
        image: RunnableImage<I>,
        command: env::Command,
    ) -> ContainerAsync<I> {
        let container = ContainerAsync {
            id,
            docker_client: Box::new(docker_client),
            image,
            command,
        };

        container.block_until_ready().await;

        container
    }

    async fn block_until_ready(&self) {
        log::debug!("Waiting for container {} to be ready", self.id);

        for condition in self.image.ready_conditions() {
            match condition {
                WaitFor::StdOutMessage { message } => self
                    .docker_client
                    .stdout_logs(&self.id)
                    .wait_for_message(&message)
                    .await
                    .unwrap(),
                WaitFor::StdErrMessage { message } => self
                    .docker_client
                    .stderr_logs(&self.id)
                    .wait_for_message(&message)
                    .await
                    .unwrap(),
                WaitFor::Duration { length } => {
                    tokio::time::sleep(length).await;
                }
                WaitFor::Healthcheck => loop {
                    use HealthStatusEnum::*;

                    let health_status = self
                        .docker_client
                        .inspect(&self.id)
                        .await
                        .state
                        .unwrap_or_else(|| panic!("Container state not available"))
                        .health
                        .unwrap_or_else(|| panic!("Health state not available"))
                        .status;

                    match health_status {
                        Some(HEALTHY) => break,
                        None | Some(EMPTY) | Some(NONE) => {
                            panic!("Healthcheck not configured for container")
                        }
                        Some(UNHEALTHY) => panic!("Healthcheck reports unhealthy"),
                        Some(STARTING) => sleep(Duration::from_millis(100)).await,
                    }
                    panic!("Healthcheck for the container is not configured");
                },
                WaitFor::Nothing => {}
            }
        }

        log::debug!("Container {} is now ready!", self.id);
    }
}

impl<I> Drop for ContainerAsync<I>
where
    I: Image,
{
    fn drop(&mut self) {
        block_on(self.drop_async())
    }
}
