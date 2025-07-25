use std::ffi::OsStr;
use std::fs;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::background_process;
use crate::local_env::{LocalEnv, NeonStorageControllerConf};
use camino::{Utf8Path, Utf8PathBuf};
use hyper0::Uri;
use nix::unistd::Pid;
use pageserver_api::controller_api::{
    NodeConfigureRequest, NodeDescribeResponse, NodeRegisterRequest,
    SafekeeperSchedulingPolicyRequest, SkSchedulingPolicy, TenantCreateRequest,
    TenantCreateResponse, TenantLocateResponse,
};
use pageserver_api::models::{
    TenantConfig, TenantConfigRequest, TimelineCreateRequest, TimelineInfo,
};
use pageserver_api::shard::TenantShardId;
use pageserver_client::mgmt_api::ResponseErrorMessageExt;
use pem::Pem;
use postgres_backend::AuthType;
use reqwest::{Method, Response};
use safekeeper_api::PgMajorVersion;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::instrument;
use url::Url;
use utils::auth::{Claims, Scope, encode_from_key_file};
use utils::id::{NodeId, TenantId};
use whoami::username;

pub struct StorageController {
    env: LocalEnv,
    private_key: Option<Pem>,
    public_key: Option<Pem>,
    client: reqwest::Client,
    config: NeonStorageControllerConf,

    // The listen port is learned when starting the storage controller,
    // hence the use of OnceLock to init it at the right time.
    listen_port: OnceLock<u16>,
}

const COMMAND: &str = "storage_controller";

const STORAGE_CONTROLLER_POSTGRES_VERSION: PgMajorVersion = PgMajorVersion::PG16;

const DB_NAME: &str = "storage_controller";

pub struct NeonStorageControllerStartArgs {
    pub instance_id: u8,
    pub base_port: Option<u16>,
    pub start_timeout: humantime::Duration,
    pub handle_ps_local_disk_loss: Option<bool>,
}

impl NeonStorageControllerStartArgs {
    pub fn with_default_instance_id(start_timeout: humantime::Duration) -> Self {
        Self {
            instance_id: 1,
            base_port: None,
            start_timeout,
            handle_ps_local_disk_loss: None,
        }
    }
}

pub struct NeonStorageControllerStopArgs {
    pub instance_id: u8,
    pub immediate: bool,
}

impl NeonStorageControllerStopArgs {
    pub fn with_default_instance_id(immediate: bool) -> Self {
        Self {
            instance_id: 1,
            immediate,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct AttachHookRequest {
    pub tenant_shard_id: TenantShardId,
    pub node_id: Option<NodeId>,
    pub generation_override: Option<i32>, // only new tenants
    pub config: Option<TenantConfig>,     // only new tenants
}

#[derive(Serialize, Deserialize)]
pub struct AttachHookResponse {
    #[serde(rename = "gen")]
    pub generation: Option<u32>,
}

#[derive(Serialize, Deserialize)]
pub struct InspectRequest {
    pub tenant_shard_id: TenantShardId,
}

#[derive(Serialize, Deserialize)]
pub struct InspectResponse {
    pub attachment: Option<(u32, NodeId)>,
}

impl StorageController {
    pub fn from_env(env: &LocalEnv) -> Self {
        // Assume all pageservers have symmetric auth configuration: this service
        // expects to use one JWT token to talk to all of them.
        let ps_conf = env
            .pageservers
            .first()
            .expect("Config is validated to contain at least one pageserver");
        let (private_key, public_key) = match ps_conf.http_auth_type {
            AuthType::Trust => (None, None),
            AuthType::NeonJWT => {
                let private_key_path = env.get_private_key_path();
                let private_key =
                    pem::parse(fs::read(private_key_path).expect("failed to read private key"))
                        .expect("failed to parse PEM file");

                // If pageserver auth is enabled, this implicitly enables auth for this service,
                // using the same credentials.
                let public_key_path =
                    camino::Utf8PathBuf::try_from(env.base_data_dir.join("auth_public_key.pem"))
                        .unwrap();

                // This service takes keys as a string rather than as a path to a file/dir: read the key into memory.
                let public_key = if std::fs::metadata(&public_key_path)
                    .expect("Can't stat public key")
                    .is_dir()
                {
                    // Our config may specify a directory: this is for the pageserver's ability to handle multiple
                    // keys.  We only use one key at a time, so, arbitrarily load the first one in the directory.
                    let mut dir =
                        std::fs::read_dir(&public_key_path).expect("Can't readdir public key path");
                    let dent = dir
                        .next()
                        .expect("Empty key dir")
                        .expect("Error reading key dir");

                    pem::parse(std::fs::read_to_string(dent.path()).expect("Can't read public key"))
                        .expect("Failed to parse PEM file")
                } else {
                    pem::parse(
                        std::fs::read_to_string(&public_key_path).expect("Can't read public key"),
                    )
                    .expect("Failed to parse PEM file")
                };
                (Some(private_key), Some(public_key))
            }
        };

        Self {
            env: env.clone(),
            private_key,
            public_key,
            client: env.create_http_client(),
            config: env.storage_controller.clone(),
            listen_port: OnceLock::default(),
        }
    }

    fn storage_controller_instance_dir(&self, instance_id: u8) -> PathBuf {
        self.env
            .base_data_dir
            .join(format!("storage_controller_{instance_id}"))
    }

    fn pid_file(&self, instance_id: u8) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(
            self.storage_controller_instance_dir(instance_id)
                .join("storage_controller.pid"),
        )
        .expect("non-Unicode path")
    }

    /// Find the directory containing postgres subdirectories, such `bin` and `lib`
    ///
    /// This usually uses STORAGE_CONTROLLER_POSTGRES_VERSION of postgres, but will fall back
    /// to other versions if that one isn't found.  Some automated tests create circumstances
    /// where only one version is available in pg_distrib_dir, such as `test_remote_extensions`.
    async fn get_pg_dir(&self, dir_name: &str) -> anyhow::Result<Utf8PathBuf> {
        const PREFER_VERSIONS: [PgMajorVersion; 5] = [
            STORAGE_CONTROLLER_POSTGRES_VERSION,
            PgMajorVersion::PG16,
            PgMajorVersion::PG15,
            PgMajorVersion::PG14,
            PgMajorVersion::PG17,
        ];

        for v in PREFER_VERSIONS {
            let path = Utf8PathBuf::from_path_buf(self.env.pg_dir(v, dir_name)?).unwrap();
            if tokio::fs::try_exists(&path).await? {
                return Ok(path);
            }
        }

        // Fall through
        anyhow::bail!(
            "Postgres directory '{}' not found in {}",
            dir_name,
            self.env.pg_distrib_dir.display(),
        );
    }

    pub async fn get_pg_bin_dir(&self) -> anyhow::Result<Utf8PathBuf> {
        self.get_pg_dir("bin").await
    }

    pub async fn get_pg_lib_dir(&self) -> anyhow::Result<Utf8PathBuf> {
        self.get_pg_dir("lib").await
    }

    /// Readiness check for our postgres process
    async fn pg_isready(&self, pg_bin_dir: &Utf8Path, postgres_port: u16) -> anyhow::Result<bool> {
        let bin_path = pg_bin_dir.join("pg_isready");
        let args = [
            "-h",
            "localhost",
            "-U",
            &username(),
            "-d",
            DB_NAME,
            "-p",
            &format!("{postgres_port}"),
        ];
        let pg_lib_dir = self.get_pg_lib_dir().await.unwrap();
        let envs = [
            ("LD_LIBRARY_PATH".to_owned(), pg_lib_dir.to_string()),
            ("DYLD_LIBRARY_PATH".to_owned(), pg_lib_dir.to_string()),
        ];
        let exitcode = Command::new(bin_path)
            .args(args)
            .envs(envs)
            .spawn()?
            .wait()
            .await?;

        Ok(exitcode.success())
    }

    /// Create our database if it doesn't exist
    ///
    /// This function is equivalent to the `diesel setup` command in the diesel CLI.  We implement
    /// the same steps by hand to avoid imposing a dependency on installing diesel-cli for developers
    /// who just want to run `cargo neon_local` without knowing about diesel.
    ///
    /// Returns the database url
    pub async fn setup_database(&self, postgres_port: u16) -> anyhow::Result<String> {
        let database_url = format!(
            "postgresql://{}@localhost:{}/{DB_NAME}",
            &username(),
            postgres_port
        );

        let pg_bin_dir = self.get_pg_bin_dir().await?;
        let createdb_path = pg_bin_dir.join("createdb");
        let pg_lib_dir = self.get_pg_lib_dir().await.unwrap();
        let envs = [
            ("LD_LIBRARY_PATH".to_owned(), pg_lib_dir.to_string()),
            ("DYLD_LIBRARY_PATH".to_owned(), pg_lib_dir.to_string()),
        ];
        let output = Command::new(&createdb_path)
            .args([
                "-h",
                "localhost",
                "-p",
                &format!("{postgres_port}"),
                "-U",
                &username(),
                "-O",
                &username(),
                DB_NAME,
            ])
            .envs(envs)
            .output()
            .await
            .expect("Failed to spawn createdb");

        if !output.status.success() {
            let stderr = String::from_utf8(output.stderr).expect("Non-UTF8 output from createdb");
            if stderr.contains("already exists") {
                tracing::info!("Database {DB_NAME} already exists");
            } else {
                anyhow::bail!("createdb failed with status {}: {stderr}", output.status);
            }
        }

        Ok(database_url)
    }

    pub async fn connect_to_database(
        &self,
        postgres_port: u16,
    ) -> anyhow::Result<(
        tokio_postgres::Client,
        tokio_postgres::Connection<tokio_postgres::Socket, tokio_postgres::tls::NoTlsStream>,
    )> {
        tokio_postgres::Config::new()
            .host("localhost")
            .port(postgres_port)
            // The user is the ambient operating system user name.
            // That is an impurity which we want to fix in => TODO https://github.com/neondatabase/neon/issues/8400
            //
            // Until we get there, use the ambient operating system user name.
            // Recent tokio-postgres versions default to this if the user isn't specified.
            // But tokio-postgres fork doesn't have this upstream commit:
            // https://github.com/sfackler/rust-postgres/commit/cb609be758f3fb5af537f04b584a2ee0cebd5e79
            // => we should rebase our fork => TODO https://github.com/neondatabase/neon/issues/8399
            .user(&username())
            .dbname(DB_NAME)
            .connect(tokio_postgres::NoTls)
            .await
            .map_err(anyhow::Error::new)
    }

    /// Wrapper for the pg_ctl binary, which we spawn as a short-lived subprocess when starting and stopping postgres
    async fn pg_ctl<I, S>(&self, args: I) -> ExitStatus
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let pg_bin_dir = self.get_pg_bin_dir().await.unwrap();
        let bin_path = pg_bin_dir.join("pg_ctl");

        let pg_lib_dir = self.get_pg_lib_dir().await.unwrap();
        let envs = [
            ("LD_LIBRARY_PATH".to_owned(), pg_lib_dir.to_string()),
            ("DYLD_LIBRARY_PATH".to_owned(), pg_lib_dir.to_string()),
        ];

        Command::new(bin_path)
            .args(args)
            .envs(envs)
            .spawn()
            .expect("Failed to spawn pg_ctl, binary_missing?")
            .wait()
            .await
            .expect("Failed to wait for pg_ctl termination")
    }

    pub async fn start(&self, start_args: NeonStorageControllerStartArgs) -> anyhow::Result<()> {
        let instance_dir = self.storage_controller_instance_dir(start_args.instance_id);
        if let Err(err) = tokio::fs::create_dir(&instance_dir).await {
            if err.kind() != std::io::ErrorKind::AlreadyExists {
                panic!("Failed to create instance dir {instance_dir:?}");
            }
        }

        if self.env.generate_local_ssl_certs {
            self.env.generate_ssl_cert(
                &instance_dir.join("server.crt"),
                &instance_dir.join("server.key"),
            )?;
        }

        let listen_url = &self.env.control_plane_api;

        let scheme = listen_url.scheme();
        let host = listen_url.host_str().unwrap();

        let (listen_port, postgres_port) = if let Some(base_port) = start_args.base_port {
            (
                base_port,
                self.config
                    .database_url
                    .expect("--base-port requires NeonStorageControllerConf::database_url")
                    .port(),
            )
        } else {
            let port = listen_url.port().unwrap();
            (port, port + 1)
        };

        self.listen_port
            .set(listen_port)
            .expect("StorageController::listen_port is only set here");

        // Do we remove the pid file on stop?
        let pg_started = self.is_postgres_running().await?;
        let pg_lib_dir = self.get_pg_lib_dir().await?;

        if !pg_started {
            // Start a vanilla Postgres process used by the storage controller for persistence.
            let pg_data_path = Utf8PathBuf::from_path_buf(self.env.base_data_dir.clone())
                .unwrap()
                .join("storage_controller_db");
            let pg_bin_dir = self.get_pg_bin_dir().await?;
            let pg_log_path = pg_data_path.join("postgres.log");

            if !tokio::fs::try_exists(&pg_data_path).await? {
                let initdb_args = [
                    "--pgdata",
                    pg_data_path.as_ref(),
                    "--username",
                    &username(),
                    "--no-sync",
                    "--no-instructions",
                ];
                tracing::info!(
                    "Initializing storage controller database with args: {:?}",
                    initdb_args
                );

                // Initialize empty database
                let initdb_path = pg_bin_dir.join("initdb");
                let mut child = Command::new(&initdb_path)
                    .envs(vec![
                        ("LD_LIBRARY_PATH".to_owned(), pg_lib_dir.to_string()),
                        ("DYLD_LIBRARY_PATH".to_owned(), pg_lib_dir.to_string()),
                    ])
                    .args(initdb_args)
                    .spawn()
                    .expect("Failed to spawn initdb");
                let status = child.wait().await?;
                if !status.success() {
                    anyhow::bail!("initdb failed with status {status}");
                }
            };

            // Write a minimal config file:
            // - Specify the port, since this is chosen dynamically
            // - Switch off fsync, since we're running on lightweight test environments and when e.g. scale testing
            //   the storage controller we don't want a slow local disk to interfere with that.
            //
            // NB: it's important that we rewrite this file on each start command so we propagate changes
            // from `LocalEnv`'s config file (`.neon/config`).
            tokio::fs::write(
                &pg_data_path.join("postgresql.conf"),
                format!("port = {postgres_port}\nfsync=off\n"),
            )
            .await?;

            println!("Starting storage controller database...");
            let db_start_args = [
                "-w",
                "-D",
                pg_data_path.as_ref(),
                "-l",
                pg_log_path.as_ref(),
                "-U",
                &username(),
                "start",
            ];
            tracing::info!(
                "Starting storage controller database with args: {:?}",
                db_start_args
            );

            let db_start_status = self.pg_ctl(db_start_args).await;
            let start_timeout: Duration = start_args.start_timeout.into();
            let db_start_deadline = Instant::now() + start_timeout;
            if !db_start_status.success() {
                return Err(anyhow::anyhow!(
                    "Failed to start postgres {}",
                    db_start_status.code().unwrap()
                ));
            }

            loop {
                if Instant::now() > db_start_deadline {
                    return Err(anyhow::anyhow!("Timed out waiting for postgres to start"));
                }

                match self.pg_isready(&pg_bin_dir, postgres_port).await {
                    Ok(true) => {
                        tracing::info!("storage controller postgres is now ready");
                        break;
                    }
                    Ok(false) => {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    Err(e) => {
                        tracing::warn!("Failed to check postgres status: {e}")
                    }
                }
            }

            self.setup_database(postgres_port).await?;
        }

        let database_url = format!("postgresql://localhost:{postgres_port}/{DB_NAME}");

        // We support running a startup SQL script to fiddle with the database before we launch storcon.
        // This is used by the test suite.
        let startup_script_path = self
            .env
            .base_data_dir
            .join("storage_controller_db.startup.sql");
        let startup_script = match tokio::fs::read_to_string(&startup_script_path).await {
            Ok(script) => {
                tokio::fs::remove_file(startup_script_path).await?;
                script
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    // always run some startup script so that this code path doesn't bit rot
                    "BEGIN; COMMIT;".to_string()
                } else {
                    anyhow::bail!("Failed to read startup script: {e}")
                }
            }
        };
        let (mut client, conn) = self.connect_to_database(postgres_port).await?;
        let conn = tokio::spawn(conn);
        let tx = client.build_transaction();
        let tx = tx.start().await?;
        tx.batch_execute(&startup_script).await?;
        tx.commit().await?;
        drop(client);
        conn.await??;

        let addr = format!("{host}:{listen_port}");
        let address_for_peers = Uri::builder()
            .scheme(scheme)
            .authority(addr.clone())
            .path_and_query("")
            .build()
            .unwrap();

        let mut args = vec![
            "--dev",
            "--database-url",
            &database_url,
            "--max-offline-interval",
            &humantime::Duration::from(self.config.max_offline).to_string(),
            "--max-warming-up-interval",
            &humantime::Duration::from(self.config.max_warming_up).to_string(),
            "--heartbeat-interval",
            &humantime::Duration::from(self.config.heartbeat_interval).to_string(),
            "--address-for-peers",
            &address_for_peers.to_string(),
        ]
        .into_iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();

        match scheme {
            "http" => args.extend(["--listen".to_string(), addr]),
            "https" => args.extend(["--listen-https".to_string(), addr]),
            _ => {
                panic!("Unexpected url scheme in control_plane_api: {scheme}");
            }
        }

        if self.config.start_as_candidate {
            args.push("--start-as-candidate".to_string());
        }

        if self.config.use_https_pageserver_api {
            args.push("--use-https-pageserver-api".to_string());
        }

        if self.config.use_https_safekeeper_api {
            args.push("--use-https-safekeeper-api".to_string());
        }

        if self.config.use_local_compute_notifications {
            args.push("--use-local-compute-notifications".to_string());
        }

        if let Some(value) = self.config.kick_secondary_downloads {
            args.push(format!("--kick-secondary-downloads={value}"));
        }

        if let Some(ssl_ca_file) = self.env.ssl_ca_cert_path() {
            args.push(format!("--ssl-ca-file={}", ssl_ca_file.to_str().unwrap()));
        }

        if let Some(private_key) = &self.private_key {
            let claims = Claims::new(None, Scope::PageServerApi);
            let jwt_token =
                encode_from_key_file(&claims, private_key).expect("failed to generate jwt token");
            args.push(format!("--jwt-token={jwt_token}"));

            let peer_claims = Claims::new(None, Scope::Admin);
            let peer_jwt_token = encode_from_key_file(&peer_claims, private_key)
                .expect("failed to generate jwt token");
            args.push(format!("--peer-jwt-token={peer_jwt_token}"));

            let claims = Claims::new(None, Scope::SafekeeperData);
            let jwt_token =
                encode_from_key_file(&claims, private_key).expect("failed to generate jwt token");
            args.push(format!("--safekeeper-jwt-token={jwt_token}"));
        }

        if let Some(public_key) = &self.public_key {
            args.push(format!("--public-key=\"{public_key}\""));
        }

        if let Some(control_plane_hooks_api) = &self.env.control_plane_hooks_api {
            args.push(format!("--control-plane-url={control_plane_hooks_api}"));
        }

        if let Some(split_threshold) = self.config.split_threshold.as_ref() {
            args.push(format!("--split-threshold={split_threshold}"))
        }

        if let Some(max_split_shards) = self.config.max_split_shards.as_ref() {
            args.push(format!("--max-split-shards={max_split_shards}"))
        }

        if let Some(initial_split_threshold) = self.config.initial_split_threshold.as_ref() {
            args.push(format!(
                "--initial-split-threshold={initial_split_threshold}"
            ))
        }

        if let Some(initial_split_shards) = self.config.initial_split_shards.as_ref() {
            args.push(format!("--initial-split-shards={initial_split_shards}"))
        }

        if let Some(lag) = self.config.max_secondary_lag_bytes.as_ref() {
            args.push(format!("--max-secondary-lag-bytes={lag}"))
        }

        if let Some(threshold) = self.config.long_reconcile_threshold {
            args.push(format!(
                "--long-reconcile-threshold={}",
                humantime::Duration::from(threshold)
            ))
        }

        args.push(format!(
            "--neon-local-repo-dir={}",
            self.env.base_data_dir.display()
        ));

        if self.env.safekeepers.iter().any(|sk| sk.auth_enabled) && self.private_key.is_none() {
            anyhow::bail!("Safekeeper set up for auth but no private key specified");
        }

        if self.config.timelines_onto_safekeepers {
            args.push("--timelines-onto-safekeepers".to_string());
        }

        // neon_local is used in test environments where we often have less than 3 safekeepers.
        if self.config.timeline_safekeeper_count.is_some() || self.env.safekeepers.len() < 3 {
            let sk_cnt = self
                .config
                .timeline_safekeeper_count
                .unwrap_or(self.env.safekeepers.len());

            args.push(format!("--timeline-safekeeper-count={sk_cnt}"));
        }

        if let Some(duration) = self.config.shard_split_request_timeout {
            args.push(format!(
                "--shard-split-request-timeout={}",
                humantime::Duration::from(duration)
            ));
        }

        let mut envs = vec![
            ("LD_LIBRARY_PATH".to_owned(), pg_lib_dir.to_string()),
            ("DYLD_LIBRARY_PATH".to_owned(), pg_lib_dir.to_string()),
        ];

        if let Some(posthog_config) = &self.config.posthog_config {
            envs.push((
                "POSTHOG_CONFIG".to_string(),
                serde_json::to_string(posthog_config)?,
            ));
        }

        println!("Starting storage controller at {scheme}://{host}:{listen_port}");

        if start_args.handle_ps_local_disk_loss.unwrap_or_default() {
            args.push("--handle-ps-local-disk-loss".to_string());
        }

        background_process::start_process(
            COMMAND,
            &instance_dir,
            &self.env.storage_controller_bin(),
            args,
            envs,
            background_process::InitialPidFile::Create(self.pid_file(start_args.instance_id)),
            &start_args.start_timeout,
            || async {
                match self.ready().await {
                    Ok(_) => Ok(true),
                    Err(_) => Ok(false),
                }
            },
        )
        .await?;

        if self.config.timelines_onto_safekeepers {
            self.register_safekeepers().await?;
        }

        Ok(())
    }

    pub async fn stop(&self, stop_args: NeonStorageControllerStopArgs) -> anyhow::Result<()> {
        background_process::stop_process(
            stop_args.immediate,
            COMMAND,
            &self.pid_file(stop_args.instance_id),
        )?;

        let storcon_instances = self.env.storage_controller_instances().await?;
        for (instance_id, instanced_dir_path) in storcon_instances {
            if instance_id == stop_args.instance_id {
                continue;
            }

            let pid_file = instanced_dir_path.join("storage_controller.pid");
            let pid = tokio::fs::read_to_string(&pid_file)
                .await
                .map_err(|err| {
                    anyhow::anyhow!("Failed to read storcon pid file at {pid_file:?}: {err}")
                })?
                .parse::<i32>()
                .expect("pid is valid i32");

            let other_proc_alive = !background_process::process_has_stopped(Pid::from_raw(pid))?;
            if other_proc_alive {
                // There is another storage controller instance running, so we return
                // and leave the database running.
                return Ok(());
            }
        }

        let pg_data_path = self.env.base_data_dir.join("storage_controller_db");

        println!("Stopping storage controller database...");
        let pg_stop_args = ["-D", &pg_data_path.to_string_lossy(), "stop"];
        let stop_status = self.pg_ctl(pg_stop_args).await;
        if !stop_status.success() {
            match self.is_postgres_running().await {
                Ok(false) => {
                    println!("Storage controller database is already stopped");
                    return Ok(());
                }
                Ok(true) => {
                    anyhow::bail!("Failed to stop storage controller database");
                }
                Err(err) => {
                    anyhow::bail!("Failed to stop storage controller database: {err}");
                }
            }
        }

        Ok(())
    }

    async fn is_postgres_running(&self) -> anyhow::Result<bool> {
        let pg_data_path = self.env.base_data_dir.join("storage_controller_db");

        let pg_status_args = ["-D", &pg_data_path.to_string_lossy(), "status"];
        let status_exitcode = self.pg_ctl(pg_status_args).await;

        // pg_ctl status returns this exit code if postgres is not running: in this case it is
        // fine that stop failed.  Otherwise it is an error that stop failed.
        const PG_STATUS_NOT_RUNNING: i32 = 3;
        const PG_NO_DATA_DIR: i32 = 4;
        const PG_STATUS_RUNNING: i32 = 0;
        match status_exitcode.code() {
            Some(PG_STATUS_NOT_RUNNING) => Ok(false),
            Some(PG_NO_DATA_DIR) => Ok(false),
            Some(PG_STATUS_RUNNING) => Ok(true),
            Some(code) => Err(anyhow::anyhow!(
                "pg_ctl status returned unexpected status code: {:?}",
                code
            )),
            None => Err(anyhow::anyhow!("pg_ctl status returned no status code")),
        }
    }

    fn get_claims_for_path(path: &str) -> anyhow::Result<Option<Claims>> {
        let category = match path.find('/') {
            Some(idx) => &path[..idx],
            None => path,
        };

        match category {
            "status" | "ready" => Ok(None),
            "control" | "debug" => Ok(Some(Claims::new(None, Scope::Admin))),
            "v1" => Ok(Some(Claims::new(None, Scope::PageServerApi))),
            _ => Err(anyhow::anyhow!("Failed to determine claims for {}", path)),
        }
    }

    /// Simple HTTP request wrapper for calling into storage controller
    async fn dispatch<RQ, RS>(
        &self,
        method: reqwest::Method,
        path: String,
        body: Option<RQ>,
    ) -> anyhow::Result<RS>
    where
        RQ: Serialize + Sized,
        RS: DeserializeOwned + Sized,
    {
        let response = self.dispatch_inner(method, path, body).await?;
        Ok(response
            .json()
            .await
            .map_err(pageserver_client::mgmt_api::Error::ReceiveBody)?)
    }

    /// Simple HTTP request wrapper for calling into storage controller
    async fn dispatch_inner<RQ>(
        &self,
        method: reqwest::Method,
        path: String,
        body: Option<RQ>,
    ) -> anyhow::Result<Response>
    where
        RQ: Serialize + Sized,
    {
        // In the special case of the `storage_controller start` subcommand, we wish
        // to use the API endpoint of the newly started storage controller in order
        // to pass the readiness check. In this scenario [`Self::listen_port`] will
        // be set (see [`Self::start`]).
        //
        // Otherwise, we infer the storage controller api endpoint from the configured
        // control plane API.
        let port = if let Some(port) = self.listen_port.get() {
            *port
        } else {
            self.env.control_plane_api.port().unwrap()
        };

        // The configured URL has the /upcall path prefix for pageservers to use: we will strip that out
        // for general purpose API access.
        let url = Url::from_str(&format!(
            "{}://{}:{port}/{path}",
            self.env.control_plane_api.scheme(),
            self.env.control_plane_api.host_str().unwrap(),
        ))
        .unwrap();

        let mut builder = self.client.request(method, url);
        if let Some(body) = body {
            builder = builder.json(&body)
        }
        if let Some(private_key) = &self.private_key {
            println!("Getting claims for path {path}");
            if let Some(required_claims) = Self::get_claims_for_path(&path)? {
                println!("Got claims {required_claims:?} for path {path}");
                let jwt_token = encode_from_key_file(&required_claims, private_key)?;
                builder = builder.header(
                    reqwest::header::AUTHORIZATION,
                    format!("Bearer {jwt_token}"),
                );
            }
        }

        let response = builder.send().await?;
        let response = response.error_from_body().await?;

        Ok(response)
    }

    /// Register the safekeepers in the storage controller
    #[instrument(skip(self))]
    async fn register_safekeepers(&self) -> anyhow::Result<()> {
        for sk in self.env.safekeepers.iter() {
            let sk_id = sk.id;
            let body = serde_json::json!({
                "id": sk_id,
                "created_at": "2023-10-25T09:11:25Z",
                "updated_at": "2024-08-28T11:32:43Z",
                "region_id": "aws-us-east-2",
                "host": "127.0.0.1",
                "port": sk.pg_port,
                "http_port": sk.http_port,
                "https_port": sk.https_port,
                "version": 5957,
                "availability_zone_id": format!("us-east-2b-{sk_id}"),
            });
            self.upsert_safekeeper(sk_id, body).await?;
            self.safekeeper_scheduling_policy(sk_id, SkSchedulingPolicy::Active)
                .await?;
        }
        Ok(())
    }

    /// Call into the attach_hook API, for use before handing out attachments to pageservers
    #[instrument(skip(self))]
    pub async fn attach_hook(
        &self,
        tenant_shard_id: TenantShardId,
        pageserver_id: NodeId,
    ) -> anyhow::Result<Option<u32>> {
        let request = AttachHookRequest {
            tenant_shard_id,
            node_id: Some(pageserver_id),
            generation_override: None,
            config: None,
        };

        let response = self
            .dispatch::<_, AttachHookResponse>(
                Method::POST,
                "debug/v1/attach-hook".to_string(),
                Some(request),
            )
            .await?;

        Ok(response.generation)
    }

    #[instrument(skip(self))]
    pub async fn upsert_safekeeper(
        &self,
        node_id: NodeId,
        request: serde_json::Value,
    ) -> anyhow::Result<()> {
        let resp = self
            .dispatch_inner::<serde_json::Value>(
                Method::POST,
                format!("control/v1/safekeeper/{node_id}"),
                Some(request),
            )
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!(
                "setting scheduling policy unsuccessful for safekeeper {node_id}: {}",
                resp.status()
            );
        }
        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn safekeeper_scheduling_policy(
        &self,
        node_id: NodeId,
        scheduling_policy: SkSchedulingPolicy,
    ) -> anyhow::Result<()> {
        self.dispatch::<SafekeeperSchedulingPolicyRequest, ()>(
            Method::POST,
            format!("control/v1/safekeeper/{node_id}/scheduling_policy"),
            Some(SafekeeperSchedulingPolicyRequest { scheduling_policy }),
        )
        .await
    }

    #[instrument(skip(self))]
    pub async fn inspect(
        &self,
        tenant_shard_id: TenantShardId,
    ) -> anyhow::Result<Option<(u32, NodeId)>> {
        let request = InspectRequest { tenant_shard_id };

        let response = self
            .dispatch::<_, InspectResponse>(
                Method::POST,
                "debug/v1/inspect".to_string(),
                Some(request),
            )
            .await?;

        Ok(response.attachment)
    }

    #[instrument(skip(self))]
    pub async fn tenant_create(
        &self,
        req: TenantCreateRequest,
    ) -> anyhow::Result<TenantCreateResponse> {
        self.dispatch(Method::POST, "v1/tenant".to_string(), Some(req))
            .await
    }

    #[instrument(skip(self))]
    pub async fn tenant_import(&self, tenant_id: TenantId) -> anyhow::Result<TenantCreateResponse> {
        self.dispatch::<(), TenantCreateResponse>(
            Method::POST,
            format!("debug/v1/tenant/{tenant_id}/import"),
            None,
        )
        .await
    }

    #[instrument(skip(self))]
    pub async fn tenant_locate(&self, tenant_id: TenantId) -> anyhow::Result<TenantLocateResponse> {
        self.dispatch::<(), _>(
            Method::GET,
            format!("debug/v1/tenant/{tenant_id}/locate"),
            None,
        )
        .await
    }

    #[instrument(skip_all, fields(node_id=%req.node_id))]
    pub async fn node_register(&self, req: NodeRegisterRequest) -> anyhow::Result<()> {
        self.dispatch::<_, ()>(Method::POST, "control/v1/node".to_string(), Some(req))
            .await
    }

    #[instrument(skip_all, fields(node_id=%req.node_id))]
    pub async fn node_configure(&self, req: NodeConfigureRequest) -> anyhow::Result<()> {
        self.dispatch::<_, ()>(
            Method::PUT,
            format!("control/v1/node/{}/config", req.node_id),
            Some(req),
        )
        .await
    }

    pub async fn node_list(&self) -> anyhow::Result<Vec<NodeDescribeResponse>> {
        self.dispatch::<(), Vec<NodeDescribeResponse>>(
            Method::GET,
            "control/v1/node".to_string(),
            None,
        )
        .await
    }

    #[instrument(skip(self))]
    pub async fn ready(&self) -> anyhow::Result<()> {
        self.dispatch::<(), ()>(Method::GET, "ready".to_string(), None)
            .await
    }

    #[instrument(skip_all, fields(%tenant_id, timeline_id=%req.new_timeline_id))]
    pub async fn tenant_timeline_create(
        &self,
        tenant_id: TenantId,
        req: TimelineCreateRequest,
    ) -> anyhow::Result<TimelineInfo> {
        self.dispatch(
            Method::POST,
            format!("v1/tenant/{tenant_id}/timeline"),
            Some(req),
        )
        .await
    }

    pub async fn set_tenant_config(&self, req: &TenantConfigRequest) -> anyhow::Result<()> {
        self.dispatch(Method::PUT, "v1/tenant/config".to_string(), Some(req))
            .await
    }
}
