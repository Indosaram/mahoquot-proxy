//! RP-1 / integration review B6: real dlopen/callback/free/dlclose ownership.
//! No fixture compilation or loading occurs until this test is executed.
#![cfg(unix)]

use mahoquot_gateway::plugins::abi::PluginError;
use mahoquot_gateway::plugins::loader::PluginHost;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

const DEADLINE: Duration = Duration::from_secs(15);
const ID: &str = "review-native-lifetime";

struct TempRoot(PathBuf);
impl Drop for TempRoot {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).expect("remove native fixture directory");
    }
}

fn busy(result: Result<bool, PluginError>) {
    let error = result.expect_err("unload must retain the live native library");
    assert_eq!(error.code, "plugin_busy");
    assert_eq!(error.http_status, Some(409));
    assert!(error.retryable);
}

// Own exactly the cc driver, its exit wait, and both pipe drains. No detached
// threads/tasks or polling; this fixture does not manage a subprocess tree.
fn compile_fixture(source: &Path, library: &Path) -> Result<Output, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("compiler runtime: {error}"))?
        .block_on(async {
            use futures::FutureExt;
            use tokio::io::AsyncReadExt;

            let mut child = tokio::process::Command::new("cc")
                .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-fPIC"])
                .arg(if cfg!(target_os = "macos") {
                    "-dynamiclib"
                } else {
                    "-shared"
                })
                .arg(source)
                .arg("-o")
                .arg(library)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|error| format!("spawn cc: {error}"))?;
            let mut stdout = child.stdout.take().expect("piped compiler stdout");
            let mut stderr = child.stderr.take().expect("piped compiler stderr");
            // Start both drains alongside the exit wait, not after exit (which
            // could deadlock on a full pipe). Keep this future across timeout.
            let drains = async move {
                let mut out = Vec::new();
                let mut err = Vec::new();
                let (out_result, err_result) =
                    tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
                out_result.map_err(|error| error.to_string())?;
                err_result.map_err(|error| error.to_string())?;
                Ok::<_, String>((out, err))
            }
            .shared();
            // Cache a completed drain result if exit waiting hits its deadline;
            // cleanup must not poll an already-completed async future again.
            let completed = tokio::time::timeout(DEADLINE, async {
                tokio::join!(child.wait(), drains.clone())
            })
            .await;
            match completed {
                Ok((status, bytes)) => {
                    let status = status.map_err(|error| format!("wait cc: {error}"))?;
                    let (stdout, stderr) = bytes.map_err(|error| format!("drain cc: {error}"))?;
                    Ok(Output {
                        status,
                        stdout,
                        stderr,
                    })
                }
                Err(_) => {
                    // Retain the exact Child after cancelling its safe wait,
                    // request termination, then bound reap AND remaining EOF.
                    let kill = child.start_kill();
                    let cleanup = tokio::time::timeout(DEADLINE, async {
                        tokio::join!(child.wait(), drains)
                    })
                    .await
                    .map(|(status, bytes)| {
                        (status, bytes.map(|(out, err)| (out.len(), err.len())))
                    });
                    Err(format!(
                        "cc deadline exceeded; termination: {kill:?}; reap/drain: {cleanup:?}"
                    ))
                }
            }
            // On a cleanup deadline the owned drains/pipes are dropped here;
            // kill_on_drop is a fallback, not a claim of successful OS reaping.
        })
}

#[test]
fn native_call_callback_and_free_survive_rejected_unload_then_quiesce_once() {
    let root = TempRoot(
        std::env::temp_dir().join(format!("review-native-plugin-{}", uuid::Uuid::new_v4())),
    );
    std::fs::create_dir(&root.0).unwrap();
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/review_20261025_plugin/lifetime.c");
    let library = root.0.join(if cfg!(target_os = "macos") {
        "lifetime.dylib"
    } else {
        "lifetime.so"
    });
    // Use the installed C toolchain; no download, new crate, or checked-in binary.
    let output = compile_fixture(&source, &library)
        .expect("installed cc must build the native ABI fixture within its deadline");
    assert!(
        output.status.success(),
        "native compiler failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let (events_tx, events) = mpsc::channel::<String>();
    let (release_tx, release) = mpsc::channel::<String>();
    let release = Mutex::new(release);
    let transcript = Arc::new(Mutex::new(Vec::<String>::new()));
    let recorded = transcript.clone();
    let host = Arc::new(PluginHost::new(Arc::new(move |id, method, bytes| {
        // Return errors rather than panic across the extern-C trampoline.
        let fail = |message: &str| PluginError {
            code: "fixture_protocol".into(),
            message: message.into(),
            retryable: false,
            http_status: Some(500),
        };
        if id != ID || bytes != b"native" {
            return Err(fail("callback identity/payload"));
        }
        recorded.lock().unwrap().push(method.to_owned());
        events_tx
            .send(method.to_owned())
            .map_err(|_| fail("event receiver closed"))?;
        if matches!(
            method,
            "native.call.entered" | "native.free.entered" | "native.quiesce.entered"
        ) {
            let signal = release
                .lock()
                .unwrap()
                .recv_timeout(DEADLINE)
                .map_err(|_| fail("gate release deadline"))?;
            if signal != method {
                return Err(fail("out-of-order gate release"));
            }
        }
        Ok(b"ack".to_vec())
    })));
    let plugin = host.load_plugin(ID, &library, "1").unwrap();
    let (mut unloaded, fixture_socket) = UnixStream::pair().unwrap();
    unloaded.set_read_timeout(Some(DEADLINE)).unwrap();
    assert_eq!(
        plugin
            .client
            .call(
                "fixture.configure",
                fixture_socket.as_raw_fd().to_string().as_bytes()
            )
            .unwrap(),
        b"configured"
    );
    drop(fixture_socket); // Native code owns a dup, closed by the library destructor.

    let expect_event = |expected: &str| {
        assert_eq!(
            events
                .recv_timeout(DEADLINE)
                .expect("native event deadline"),
            expected
        );
    };
    let (call_tx, call_done) = mpsc::channel();
    let caller = plugin.client.clone();
    let call_thread = std::thread::spawn(move || {
        call_tx.send(caller.call("fixture.active", b"{}")).unwrap();
    });
    expect_event("native.call.entered");
    busy(host.try_unload_plugin(ID));
    assert!(host.get_plugin(ID).is_some());
    assert_eq!(
        plugin
            .active_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(plugin.client.call("fixture.ping", b"{}").unwrap(), b"pong");
    release_tx.send("native.call.entered".into()).unwrap();
    expect_event("native.call.resumed");
    expect_event("native.free.entered");
    busy(host.try_unload_plugin(ID));
    assert!(host.get_plugin(ID).is_some());
    assert_eq!(
        plugin
            .active_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    release_tx.send("native.free.entered".into()).unwrap();
    expect_event("native.free.resumed");
    assert_eq!(
        call_done.recv_timeout(DEADLINE).unwrap().unwrap(),
        b"native-owned-response"
    );
    call_thread.join().unwrap();
    assert_eq!(plugin.client.call("fixture.ping", b"{}").unwrap(), b"pong");
    assert_eq!(
        plugin
            .active_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );

    let (unload_tx, unload_done) = mpsc::channel();
    let draining = host.clone();
    let unload_thread = std::thread::spawn(move || {
        unload_tx.send(draining.try_unload_plugin(ID)).unwrap();
    });
    expect_event("native.quiesce.entered");
    // This waits for the production two-second quiesce timeout, not a test sleep.
    // The gate cannot complete until we explicitly release it below.
    busy(unload_done.recv_timeout(DEADLINE).unwrap());
    unload_thread.join().unwrap();
    assert!(host.get_plugin(ID).is_some());
    assert_eq!(
        plugin
            .client
            .call("fixture.active", b"{}")
            .unwrap_err()
            .code,
        "plugin_busy"
    );
    // Subscribe to the real unload operation before releasing native quiesce.
    // The retry waits on the lifecycle Condvar until the result is published;
    // native.quiesce.free is still inside free_buffer, not a completion signal.
    let (retry_tx, retry_done) = mpsc::channel();
    let completing = host.clone();
    let retry_thread = std::thread::spawn(move || {
        retry_tx.send(completing.try_unload_plugin(ID)).unwrap();
    });
    release_tx.send("native.quiesce.entered".into()).unwrap();
    expect_event("native.quiesce.resumed");
    expect_event("native.quiesce.free");
    assert!(retry_done
        .recv_timeout(DEADLINE)
        .expect("unload completion deadline")
        .unwrap());
    retry_thread.join().unwrap();
    expect_event("native.shutdown");
    let mut destructor_bytes = Vec::new();
    unloaded
        .read_to_end(&mut destructor_bytes)
        .expect("dlclose destructor and EOF deadline");
    assert_eq!(destructor_bytes, b"D");
    assert!(host.get_plugin(ID).is_none());
    assert!(!host.inner.active_versions.read().unwrap().contains_key(ID));
    assert!(!host.try_unload_plugin(ID).unwrap());
    assert_eq!(
        plugin
            .client
            .call("fixture.active", b"{}")
            .unwrap_err()
            .code,
        "plugin_busy"
    );
    drop(plugin); // A stale client Arc must not execute shutdown a second time.
    assert_eq!(
        *transcript.lock().unwrap(),
        [
            "native.call.entered",
            "native.call.resumed",
            "native.free.entered",
            "native.free.resumed",
            "native.quiesce.entered",
            "native.quiesce.resumed",
            "native.quiesce.free",
            "native.shutdown",
        ]
    );
}
