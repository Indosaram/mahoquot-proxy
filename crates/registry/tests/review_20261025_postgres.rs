//! Real, disposable PostgreSQL regressions. No ambient DSN or existing cluster is used.
//! Run this target only at the integrated gate; missing PostgreSQL is a failure, not a skip.
use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use mahoquot_registry::store::{PostgresStore, StorageBackend, StorageManager};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tokio::time::timeout;

const DEADLINE: Duration = Duration::from_secs(30);
const CLEANUP: Duration = Duration::from_secs(10);

fn executable(program: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join(program))
        .find(|path| {
            path.is_file()
                && fs::metadata(path)
                    .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
        })
        .map(|path| fs::canonicalize(path).expect("resolve installed executable"))
}

async fn postgres_bin() -> PathBuf {
    if let Some(postgres) = executable("postgres") {
        return postgres.parent().unwrap().to_path_buf();
    }
    // Distribution packages often put server binaries outside PATH.
    let config = executable("pg_config")
        .expect("fixture requires installed postgres on PATH or pg_config --bindir");
    let output = timeout(
        DEADLINE,
        Command::new(config)
            .arg("--bindir")
            .env_clear()
            .env("LC_ALL", "C")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("pg_config deadline")
    .expect("run installed pg_config");
    assert!(output.status.success(), "pg_config failed: {output:?}");
    PathBuf::from(
        String::from_utf8(output.stdout)
            .expect("pg_config UTF-8")
            .trim(),
    )
}

struct Cluster {
    root: PathBuf,
    bin: PathBuf,
    kill: PathBuf,
    port: u16,
    child: Option<Child>,
    logs: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl Cluster {
    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(self.bin.join(program));
        command
            .env_clear()
            .env(
                "PATH",
                std::env::join_paths([
                    self.bin.as_path(),
                    std::path::Path::new("/usr/bin"),
                    std::path::Path::new("/bin"),
                ])
                .unwrap(),
            )
            .env("HOME", &self.root)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        command
    }

    async fn start() -> Self {
        let bin = postgres_bin().await;
        for program in ["initdb", "postgres", "pg_ctl"] {
            let path = bin.join(program);
            assert!(
                path.is_file()
                    && fs::metadata(&path)
                        .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0),
                "fixture requires installed executable {}",
                path.display()
            );
        }
        let kill = executable("kill").expect("fixture cleanup requires installed kill on PATH");
        let root = std::env::temp_dir().join(format!(
            "mahoquot-rp2-pg-{}-{:032x}",
            std::process::id(),
            rand::random::<u128>()
        ));
        fs::create_dir(&root).expect("create uniquely owned fixture directory");
        let mut fixture = Self {
            root,
            bin,
            kill,
            port: 0,
            child: None,
            logs: None,
        };
        // This is finite port selection, not readiness polling. Hold the reservation
        // through initdb; release only when PostgreSQL must take over the socket.
        let reservation = (18840..=18899)
            .find_map(|port| TcpListener::bind(("127.0.0.1", port)).ok())
            .expect("no free fixture port in 18840-18899");
        fixture.port = reservation.local_addr().unwrap().port();
        let init_log = fs::File::create(fixture.root.join("initdb.log")).unwrap();
        let mut initialize = fixture.command("initdb");
        initialize.as_std_mut().process_group(0);
        let mut init = initialize
            .arg("-D")
            .arg(fixture.root.join("data"))
            .args([
                "--username=fixture_admin",
                "--auth=trust",
                "--encoding=UTF8",
                "--no-locale",
            ])
            .stdout(init_log.try_clone().unwrap())
            .stderr(init_log)
            .spawn()
            .expect("spawn installed initdb");
        let initialized = timeout(DEADLINE, init.wait()).await;
        if !matches!(&initialized, Ok(Ok(status)) if status.success()) {
            if let Some(pid) = init.id() {
                // initdb can have a bootstrap postgres child on a timeout.
                let status = timeout(
                    CLEANUP,
                    Command::new(&fixture.kill)
                        .args(["-KILL", "--", &format!("-{pid}")])
                        .env_clear()
                        .stdin(Stdio::null())
                        .kill_on_drop(true)
                        .status(),
                )
                .await
                .expect("initdb group kill deadline")
                .expect("kill initdb group");
                assert!(status.success(), "initdb group kill failed: {status}");
            }
            timeout(CLEANUP, init.wait())
                .await
                .expect("initdb cleanup deadline")
                .expect("reap initdb");
            panic!(
                "initdb failed: {initialized:?}; {}",
                fs::read_to_string(fixture.root.join("initdb.log")).unwrap()
            );
        }

        // Subscribe before spawning. The pipe retains even an immediately emitted
        // readiness record; no SQL/TCP probe or request precedes this exact event.
        let (ready_tx, ready_rx) = oneshot::channel();
        let log_file = fs::File::create(fixture.root.join("postgres.log")).unwrap();
        drop(reservation);
        let mut server = fixture.command("postgres");
        // Own all backends even if startup fails before postmaster.pid exists.
        server.as_std_mut().process_group(0);
        let child = server
            .arg("-D")
            .arg(fixture.root.join("data"))
            .arg("-p")
            .arg(fixture.port.to_string())
            .args([
                "-h",
                "127.0.0.1",
                "-c",
                "unix_socket_directories=",
                "-c",
                "logging_collector=off",
                "-c",
                "log_destination=stderr",
                "-c",
                "log_line_prefix=",
                "-c",
                "log_min_messages=log",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn installed postgres");
        fixture.child = Some(child);
        let stderr = fixture.child.as_mut().unwrap().stderr.take().unwrap();
        fixture.logs = Some(tokio::spawn(async move {
            use std::io::Write;
            let mut log_file = log_file;
            let mut lines = BufReader::new(stderr).lines();
            let mut ready_tx = Some(ready_tx);
            while let Some(line) = lines.next_line().await? {
                writeln!(log_file, "{line}")?;
                if line == "LOG:  database system is ready to accept connections" {
                    if let Some(sender) = ready_tx.take() {
                        // A startup timeout can drop the receiver; still drain logs.
                        let _receiver_closed = sender.send(());
                    }
                }
            }
            Ok(())
        }));
        let ready = timeout(DEADLINE, ready_rx).await;
        assert!(
            matches!(ready, Ok(Ok(()))),
            "PostgreSQL readiness failed: {ready:?}; {}",
            fs::read_to_string(fixture.root.join("postgres.log")).unwrap()
        );
        fixture
    }

    fn dsn(&self, role: &str) -> String {
        format!(
            "postgres://{role}@127.0.0.1:{}/postgres?sslmode=disable",
            self.port
        )
    }

    async fn pool(&self, role: &str) -> PgPool {
        PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(3))
            .connect(&self.dsn(role))
            .await
            .expect("connect fixture role")
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        // Tests use multi-thread runtimes because the production store blocks in
        // place. Cleanup runs on assertion unwinds too, with a bounded reap wait.
        let cleanup: Result<(), String> = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                if let Some(mut child) = self.child.take() {
                    // Let the postmaster stop its backends first. If startup has
                    // not produced postmaster.pid, kill the owned startup group.
                    let mut control = self
                        .command("pg_ctl")
                        .arg("-D")
                        .arg(self.root.join("data"))
                        .args(["stop", "-m", "immediate", "-W"])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .map_err(|e| format!("spawn pg_ctl: {e}"))?;
                    let controlled = timeout(CLEANUP, control.wait()).await;
                    if matches!(controlled, Ok(Ok(status)) if status.success()) {
                        timeout(CLEANUP, child.wait())
                            .await
                            .map_err(|e| format!("postgres reap deadline: {e}"))?
                            .map_err(|e| format!("reap postgres: {e}"))?;
                    } else {
                        control
                            .start_kill()
                            .map_err(|e| format!("kill pg_ctl: {e}"))?;
                        timeout(CLEANUP, control.wait())
                            .await
                            .map_err(|e| format!("pg_ctl reap deadline: {e}"))?
                            .map_err(|e| format!("reap pg_ctl: {e}"))?;
                        let group = format!("-{}", child.id().expect("unreaped postgres PID"));
                        let mut stop = Command::new(&self.kill)
                            .args(["-KILL", "--", &group])
                            .env_clear()
                            .stdin(Stdio::null())
                            .kill_on_drop(true)
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .spawn()
                            .map_err(|e| format!("spawn cluster stop: {e}"))?;
                        let status = timeout(CLEANUP, stop.wait())
                            .await
                            .map_err(|e| format!("cluster stop deadline: {e}"))?
                            .map_err(|e| format!("cluster stop: {e}"))?;
                        if !status.success() {
                            // A startup failure may have already exited the group.
                            child
                                .start_kill()
                                .map_err(|e| format!("kill postgres: {e}"))?;
                        }
                        timeout(CLEANUP, child.wait())
                            .await
                            .map_err(|e| format!("postgres reap deadline: {e}"))?
                            .map_err(|e| format!("reap postgres: {e}"))?;
                    }
                }
                if let Some(mut logs) = self.logs.take() {
                    match timeout(CLEANUP, &mut logs).await {
                        Ok(result) => result
                            .map_err(|e| e.to_string())?
                            .map_err(|e| e.to_string())?,
                        Err(error) => {
                            logs.abort();
                            return Err(format!("postgres log drain deadline: {error}"));
                        }
                    }
                }
                fs::remove_dir_all(&self.root).map_err(|e| e.to_string())
            })
        });
        if let Err(error) = cleanup {
            if std::thread::panicking() {
                eprintln!(
                    "fixture cleanup failed; retained {}: {error}",
                    self.root.display()
                );
            } else {
                panic!(
                    "fixture cleanup failed; retained {}: {error}",
                    self.root.display()
                );
            }
        }
    }
}

async fn sql(pool: &PgPool, statement: &str) {
    timeout(DEADLINE, sqlx::query(statement).execute(pool))
        .await
        .expect("fixture SQL deadline")
        .expect("fixture SQL");
}

// Relocated from store.rs::postgres_store_live_db_when_dsn_present. Preserve
// every config put/get/delete assertion without live DSNs or a skip branch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_store_disposable_cluster_crud() {
    let fixture = Cluster::start().await;
    let pg = PostgresStore::new(fixture.dsn("fixture_admin"), Some("test_schema".into()));
    pg.put("config/config.yaml", b"port: 18801\n").unwrap();
    assert_eq!(
        pg.get("config/config.yaml").unwrap().unwrap(),
        b"port: 18801\n"
    );
    pg.delete("config/config.yaml").unwrap();
    assert!(pg.get("config/config.yaml").unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_credential_updates_and_same_spool_sync_preserve_conflict_detection() {
    let fixture = Cluster::start().await;
    let store = Arc::new(PostgresStore::try_new(fixture.dsn("fixture_admin"), None).unwrap());
    let spool = fixture.root.join("repeated-spool");
    let manager = StorageManager::new(store.clone(), &spool).unwrap();
    for generation in 0..3 {
        let credential = json!({"token": "fixture", "generation": generation, "disabled": false});
        manager
            .put(
                "auths/account.json",
                &serde_json::to_vec_pretty(&credential).unwrap(),
            )
            .unwrap();
        manager.sync_all_to_spool().unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(spool.join("auths/account.json")).unwrap())
                .unwrap(),
            credential
        );
    }
    store
        .put("auths/account.json", br#"{"token":"other"}"#)
        .unwrap();
    assert!(matches!(
        manager.put("auths/account.json", b"{}"),
        Err(mahoquot_registry::store::StoreError::Conflict(_))
    ));
    assert!(manager.sync_all_to_spool().is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn denied_schema_creation_never_acknowledges_durability() {
    let fixture = Cluster::start().await;
    let admin = fixture.pool("fixture_admin").await;
    sql(&admin, "CREATE ROLE schema_denied LOGIN").await;
    sql(&admin, "REVOKE CREATE ON DATABASE postgres FROM PUBLIC").await;
    let denied = fixture.pool("schema_denied").await;
    let error = sqlx::query("CREATE SCHEMA denied_schema")
        .execute(&denied)
        .await
        .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("42501")
    );
    assert!(
        PostgresStore::try_new(fixture.dsn("schema_denied"), Some("denied_schema".into())).is_err()
    );
    let store = PostgresStore::new(fixture.dsn("schema_denied"), Some("denied_schema".into()));
    assert!(store
        .put("auths/denied.json", br#"{"token":"not-durable"}"#)
        .is_err());
    assert!(store.get("auths/denied.json").is_err());
    assert!(store.delete("auths/denied.json").is_err());
    assert!(store.list("").is_err());
    assert!(store
        .save_cooldown("denied", "model", b"{}", false)
        .is_err());
    assert!(store.load_cooldowns().is_err());
    denied.close().await;
    admin.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn denied_second_inventory_query_preserves_exact_spool() {
    let fixture = Cluster::start().await;
    let admin = fixture.pool("fixture_admin").await;
    let owner = PostgresStore::try_new(fixture.dsn("fixture_admin"), None).unwrap();
    owner.put("config/config.yaml", b"remote: true\n").unwrap();
    owner
        .put("auths/remote.json", br#"{"token":"remote"}"#)
        .unwrap();
    sql(&admin, "CREATE ROLE inventory_reader LOGIN").await;
    sql(
        &admin,
        "GRANT USAGE, CREATE ON SCHEMA public TO inventory_reader",
    )
    .await;
    sql(
        &admin,
        "GRANT SELECT ON config_store, auth_store TO inventory_reader",
    )
    .await;
    let reader = Arc::new(PostgresStore::try_new(fixture.dsn("inventory_reader"), None).unwrap());
    sql(
        &admin,
        "REVOKE CREATE ON SCHEMA public FROM inventory_reader",
    )
    .await;
    assert_eq!(
        reader.list("").unwrap(),
        vec!["auths/remote.json", "config/config.yaml"]
    );
    let spool = fixture.root.join("spool");
    fs::create_dir_all(spool.join("auths")).unwrap();
    fs::create_dir_all(spool.join("config")).unwrap();
    let credential = b"{\"token\":\"local-exact\"}\n";
    // Match the first inventory's bytes so an incorrectly partial inventory
    // reaches deletion, rather than being masked by a spool-conflict error.
    let config = b"remote: true\n";
    fs::write(spool.join("auths/local.json"), credential).unwrap();
    fs::write(spool.join("config/config.yaml"), config).unwrap();
    let manager = StorageManager::new(reader.clone(), &spool).unwrap();
    sql(&admin, "REVOKE SELECT ON auth_store FROM inventory_reader").await;
    let probe = fixture.pool("inventory_reader").await;
    // Establish that the first list query succeeds and the second specifically
    // fails for insufficient privilege, rather than connection/schema failure.
    let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM config_store")
        .fetch_all(&probe)
        .await
        .unwrap();
    assert_eq!(ids, vec!["config/config.yaml"]);
    let error = sqlx::query("SELECT id FROM auth_store")
        .fetch_all(&probe)
        .await
        .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("42501")
    );
    assert!(reader.list("").is_err());
    assert!(manager.sync_all_to_spool().is_err());
    assert_eq!(
        fs::read(spool.join("auths/local.json")).unwrap(),
        credential
    );
    assert_eq!(fs::read(spool.join("config/config.yaml")).unwrap(), config);
    assert!(!spool.join("auths/remote.json").exists());
    probe.close().await;
    admin.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopened_store_restores_credentials_and_cooldowns_without_old_spool() {
    let fixture = Cluster::start().await;
    let credential = json!({"access_token": "fixture-rotated", "refresh_token": "fixture-refresh", "generation": 2});
    let cooldown = json!({"until": 4102444800_u64, "reason": "fixture"});
    let old_spool = fixture.root.join("old-spool");
    {
        let store = Arc::new(
            PostgresStore::try_new(fixture.dsn("fixture_admin"), Some("durability".into()))
                .unwrap(),
        );
        let manager = StorageManager::new(store.clone(), &old_spool).unwrap();
        manager
            .put(
                "auths/account.json",
                &serde_json::to_vec(&credential).unwrap(),
            )
            .unwrap();
        manager
            .put("config/config.yaml", b"fixture: durable\n")
            .unwrap();
        store
            .save_cooldown(
                "account",
                "model-a",
                &serde_json::to_vec(&cooldown).unwrap(),
                false,
            )
            .unwrap();
        store
            .save_cooldown("account", "model-deleted", b"{}", false)
            .unwrap();
        store
            .save_cooldown("account", "model-deleted", b"{}", true)
            .unwrap();
    }
    fs::remove_dir_all(&old_spool).unwrap();
    let reopened = Arc::new(
        PostgresStore::try_new(fixture.dsn("fixture_admin"), Some("durability".into())).unwrap(),
    );
    let fresh_spool = fixture.root.join("fresh-spool");
    let manager = StorageManager::new(reopened.clone(), &fresh_spool).unwrap();
    manager.sync_all_to_spool().unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(fresh_spool.join("auths/account.json")).unwrap())
            .unwrap(),
        credential
    );
    assert_eq!(
        fs::read(fresh_spool.join("config/config.yaml")).unwrap(),
        b"fixture: durable\n"
    );
    let rows = reopened.load_cooldowns().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!((&rows[0].0[..], &rows[0].1[..]), ("account", "model-a"));
    assert_eq!(
        serde_json::from_slice::<Value>(&rows[0].2).unwrap(),
        cooldown
    );
}
