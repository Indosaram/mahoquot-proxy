use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use crate::plugins::abi::*;

pub trait PluginClient: Send + Sync {
    fn call(&self, method: &str, request: &[u8]) -> Result<Vec<u8>, PluginError>;
    fn shutdown(&self);
}

pub struct DynamicLibraryClient {
    handle: *mut c_void,
    api: CliproxyPluginApi,
    host_api: *mut CliproxyHostApi,
    host_ctx: *mut c_void,
    closed: AtomicBool,
    active_calls: Arc<AtomicUsize>,
}

unsafe impl Send for DynamicLibraryClient {}
unsafe impl Sync for DynamicLibraryClient {}

impl PluginClient for DynamicLibraryClient {
    fn call(&self, method: &str, request: &[u8]) -> Result<Vec<u8>, PluginError> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(PluginError {
                code: "client_closed".to_string(),
                message: "plugin client is closed".to_string(),
                retryable: false,
                http_status: Some(500),
            });
        }
        let call_fn = match self.api.call {
            Some(f) => f,
            None => {
                return Err(PluginError {
                    code: "client_closed".to_string(),
                    message: "plugin client has no call entry point".to_string(),
                    retryable: false,
                    http_status: Some(500),
                });
            }
        };

        self.active_calls.fetch_add(1, Ordering::SeqCst);
        let _guard = CallGuard(self.active_calls.clone());

        let c_method = match CString::new(method) {
            Ok(c) => c,
            Err(e) => {
                return Err(PluginError {
                    code: "invalid_method".to_string(),
                    message: format!("invalid method string: {e}"),
                    retryable: false,
                    http_status: Some(400),
                });
            }
        };

        let mut response = CliproxyBuffer::default();
        let rc = unsafe {
            call_fn(
                c_method.as_ptr(),
                if request.is_empty() {
                    std::ptr::null()
                } else {
                    request.as_ptr()
                },
                request.len(),
                &mut response,
            )
        };

        let bytes = if !response.ptr.is_null() && response.len > 0 {
            let slice =
                unsafe { std::slice::from_raw_parts(response.ptr as *const u8, response.len) };
            slice.to_vec()
        } else {
            Vec::new()
        };

        if !response.ptr.is_null() {
            if let Some(free_fn) = self.api.free_buffer {
                unsafe { free_fn(response.ptr, response.len) };
            }
        }

        if rc != 0 {
            if let Ok(env) = serde_json::from_slice::<Envelope>(&bytes) {
                if !env.ok {
                    if let Some(err) = env.error {
                        return Err(err);
                    }
                }
            }
            return Err(PluginError {
                code: "plugin_call_failed".to_string(),
                message: format!(
                    "plugin call {method} returned {rc}: {}",
                    String::from_utf8_lossy(&bytes)
                ),
                retryable: false,
                http_status: Some(502),
            });
        }

        Ok(bytes)
    }

    fn shutdown(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(shutdown_fn) = self.api.shutdown {
            unsafe { shutdown_fn() };
        }
        if !self.handle.is_null() {
            unsafe {
                #[cfg(unix)]
                libc::dlclose(self.handle);
            }
        }
        if !self.host_api.is_null() {
            unsafe { drop(Box::from_raw(self.host_api)) };
        }
        if !self.host_ctx.is_null() {
            unsafe { drop(Box::from_raw(self.host_ctx as *mut HostContextData)) };
        }
    }
}

impl Drop for DynamicLibraryClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct CallGuard(Arc<AtomicUsize>);

impl Drop for CallGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

pub struct HostContextData {
    pub callback_id: usize,
    pub plugin_id: String,
    pub host: Arc<PluginHostInner>,
}

unsafe extern "C" fn host_call_trampoline(
    host_ctx: *mut c_void,
    method: *const c_char,
    request: *const u8,
    request_len: usize,
    response: *mut CliproxyBuffer,
) -> i32 {
    if response.is_null() {
        return 1;
    }
    unsafe {
        (*response).ptr = std::ptr::null_mut();
        (*response).len = 0;
    }
    if host_ctx.is_null() || method.is_null() {
        return 1;
    }
    let ctx = unsafe { &*(host_ctx as *const HostContextData) };
    let method_str = match unsafe { CStr::from_ptr(method) }.to_str() {
        Ok(s) => s,
        Err(_) => return 1,
    };
    let req_bytes = if !request.is_null() && request_len > 0 {
        unsafe { std::slice::from_raw_parts(request, request_len) }
    } else {
        &[]
    };

    let result = ctx
        .host
        .handle_host_callback(&ctx.plugin_id, method_str, req_bytes);
    let out_bytes = match result {
        Ok(b) => b,
        Err(e) => {
            let env = Envelope::error(e.code, e.message, e.http_status);
            serde_json::to_vec(&env).unwrap_or_default()
        }
    };

    if !out_bytes.is_empty() {
        let len = out_bytes.len();
        let layout = std::alloc::Layout::from_size_align(len, 1).unwrap();
        let ptr = unsafe { std::alloc::alloc(layout) };
        if ptr.is_null() {
            return 1;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(out_bytes.as_ptr(), ptr, len);
            (*response).ptr = ptr as *mut c_void;
            (*response).len = len;
        }
    }
    0
}

unsafe extern "C" fn host_free_trampoline(ptr: *mut c_void, len: usize) {
    if !ptr.is_null() && len > 0 {
        let layout = std::alloc::Layout::from_size_align(len, 1).unwrap();
        unsafe { std::alloc::dealloc(ptr as *mut u8, layout) };
    }
}

pub struct LoadedPlugin {
    pub id: String,
    pub path: PathBuf,
    pub version: String,
    pub name: String,
    pub schema_version: u32,
    pub metadata: serde_json::Value,
    pub capabilities: serde_json::Value,
    pub client: Arc<dyn PluginClient>,
    pub active_calls: Arc<AtomicUsize>,
}

pub type HostCallbackHandler =
    Arc<dyn Fn(&str, &str, &[u8]) -> Result<Vec<u8>, PluginError> + Send + Sync>;

pub struct PluginHostInner {
    pub loaded: RwLock<HashMap<String, Arc<LoadedPlugin>>>,
    pub active_versions: RwLock<HashMap<String, String>>,
    pub load_errors: RwLock<HashMap<String, (PathBuf, PluginError)>>,
    pub host_callback_handler: HostCallbackHandler,
}

impl PluginHostInner {
    pub fn new(handler: HostCallbackHandler) -> Self {
        Self {
            loaded: RwLock::new(HashMap::new()),
            active_versions: RwLock::new(HashMap::new()),
            load_errors: RwLock::new(HashMap::new()),
            host_callback_handler: handler,
        }
    }

    pub fn handle_host_callback(
        &self,
        plugin_id: &str,
        method: &str,
        request: &[u8],
    ) -> Result<Vec<u8>, PluginError> {
        (self.host_callback_handler)(plugin_id, method, request)
    }

    pub fn quiesce_plugin(&self, id: &str, timeout_ms: u64) -> bool {
        let plugin = {
            let loaded = self.loaded.read().unwrap();
            loaded.get(id).cloned()
        };
        let plugin = match plugin {
            Some(p) => p,
            None => return true,
        };

        let _ = plugin.client.call(METHOD_PLUGIN_QUIESCE, b"{}");

        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_millis(timeout_ms);
        while plugin.active_calls.load(Ordering::SeqCst) > 0 {
            if start.elapsed() >= timeout {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        true
    }

    pub fn unload_plugin(&self, id: &str) -> bool {
        self.quiesce_plugin(id, 2000);
        let mut loaded = self.loaded.write().unwrap();
        if let Some(plugin) = loaded.remove(id) {
            plugin.client.shutdown();
            let mut versions = self.active_versions.write().unwrap();
            versions.remove(id);
            true
        } else {
            false
        }
    }

    pub fn is_plugin_busy(&self, id: &str) -> bool {
        if std::env::var("MAHOQUOT_QA_PLUGIN_BUSY")
            .map(|v| v == "1" || v == id)
            .unwrap_or(false)
        {
            return true;
        }
        let loaded = self.loaded.read().unwrap();
        if let Some(p) = loaded.get(id) {
            p.active_calls.load(Ordering::SeqCst) > 0
        } else {
            false
        }
    }

    pub fn record_load_error(&self, id: &str, path: PathBuf, err: PluginError) {
        let mut errors = self.load_errors.write().unwrap();
        errors.insert(id.to_string(), (path, err));
    }

    pub fn list_load_errors(&self) -> Vec<(String, PathBuf, PluginError)> {
        let errors = self.load_errors.read().unwrap();
        errors
            .iter()
            .map(|(id, (p, e))| (id.clone(), p.clone(), e.clone()))
            .collect()
    }

    pub fn clear_load_error(&self, id: &str) {
        let mut errors = self.load_errors.write().unwrap();
        errors.remove(id);
    }
}

pub struct PluginHost {
    pub inner: Arc<PluginHostInner>,
}

impl PluginHost {
    pub fn new(handler: HostCallbackHandler) -> Self {
        Self {
            inner: Arc::new(PluginHostInner::new(handler)),
        }
    }

    pub fn is_plugin_busy(&self, id: &str) -> bool {
        self.inner.is_plugin_busy(id)
    }

    pub fn record_load_error(&self, id: &str, path: PathBuf, err: PluginError) {
        self.inner.record_load_error(id, path, err);
    }

    pub fn list_load_errors(&self) -> Vec<(String, PathBuf, PluginError)> {
        self.inner.list_load_errors()
    }

    pub fn clear_load_error(&self, id: &str) {
        self.inner.clear_load_error(id);
    }

    pub fn unload_plugin(&self, id: &str) -> bool {
        self.inner.clear_load_error(id);
        self.inner.unload_plugin(id)
    }

    pub fn load_plugin(
        &self,
        id: &str,
        path: &Path,
        version: &str,
    ) -> Result<Arc<LoadedPlugin>, PluginError> {
        let active_calls = Arc::new(AtomicUsize::new(0));
        let path_str = path.to_string_lossy().to_string();

        let c_path = CString::new(path_str.as_str()).map_err(|e| PluginError {
            code: "invalid_path".to_string(),
            message: format!("invalid library path: {e}"),
            retryable: false,
            http_status: Some(500),
        })?;

        #[cfg(unix)]
        let handle = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        #[cfg(not(unix))]
        let handle: *mut c_void = std::ptr::null_mut();

        if handle.is_null() {
            #[cfg(unix)]
            let err_msg = unsafe {
                let err_ptr = libc::dlerror();
                if !err_ptr.is_null() {
                    CStr::from_ptr(err_ptr).to_string_lossy().to_string()
                } else {
                    "unknown dlopen error".to_string()
                }
            };
            #[cfg(not(unix))]
            let err_msg = "unsupported platform".to_string();

            return Err(PluginError {
                code: "dlopen_failed".to_string(),
                message: format!("dlopen {}: {}", path.display(), err_msg),
                retryable: false,
                http_status: Some(500),
            });
        }

        let sym_name = CString::new("cliproxy_plugin_init").unwrap();
        #[cfg(unix)]
        let init_sym = unsafe { libc::dlsym(handle, sym_name.as_ptr()) };
        #[cfg(not(unix))]
        let init_sym: *mut c_void = std::ptr::null_mut();

        if init_sym.is_null() {
            #[cfg(unix)]
            unsafe {
                libc::dlclose(handle)
            };
            return Err(PluginError {
                code: "missing_symbol".to_string(),
                message: format!("missing cliproxy_plugin_init in {}", path.display()),
                retryable: false,
                http_status: Some(500),
            });
        }

        let ctx_data = Box::into_raw(Box::new(HostContextData {
            callback_id: 1,
            plugin_id: id.to_string(),
            host: self.inner.clone(),
        }));

        let host_api = Box::into_raw(Box::new(CliproxyHostApi {
            abi_version: ABI_VERSION,
            host_ctx: ctx_data as *mut c_void,
            call: Some(host_call_trampoline),
            free_buffer: Some(host_free_trampoline),
        }));

        let mut plugin_api = CliproxyPluginApi::default();
        let init_fn: CliproxyPluginInitFn = unsafe { std::mem::transmute(init_sym) };
        let rc = unsafe { init_fn(host_api, &mut plugin_api) };

        if rc != 0 {
            unsafe {
                drop(Box::from_raw(host_api));
                drop(Box::from_raw(ctx_data));
                #[cfg(unix)]
                libc::dlclose(handle);
            }
            return Err(PluginError {
                code: "init_failed".to_string(),
                message: format!("cliproxy_plugin_init returned {rc}"),
                retryable: false,
                http_status: Some(500),
            });
        }

        if plugin_api.abi_version != ABI_VERSION {
            let ver = plugin_api.abi_version;
            if let Some(shutdown_fn) = plugin_api.shutdown {
                unsafe { shutdown_fn() };
            }
            unsafe {
                drop(Box::from_raw(host_api));
                drop(Box::from_raw(ctx_data));
                #[cfg(unix)]
                libc::dlclose(handle);
            }
            return Err(PluginError {
                code: "abi_mismatch".to_string(),
                message: format!(
                    "plugin ABI version {ver} is not supported (expected {ABI_VERSION})"
                ),
                retryable: false,
                http_status: Some(500),
            });
        }

        if plugin_api.call.is_none() || plugin_api.free_buffer.is_none() {
            if let Some(shutdown_fn) = plugin_api.shutdown {
                unsafe { shutdown_fn() };
            }
            unsafe {
                drop(Box::from_raw(host_api));
                drop(Box::from_raw(ctx_data));
                #[cfg(unix)]
                libc::dlclose(handle);
            }
            return Err(PluginError {
                code: "incomplete_api".to_string(),
                message: "plugin function table is incomplete".to_string(),
                retryable: false,
                http_status: Some(500),
            });
        }

        let client = Arc::new(DynamicLibraryClient {
            handle,
            api: plugin_api,
            host_api,
            host_ctx: ctx_data as *mut c_void,
            closed: AtomicBool::new(false),
            active_calls: active_calls.clone(),
        });

        self.register_plugin_client(id, path, version, client, active_calls)
    }

    pub fn register_plugin_client(
        &self,
        id: &str,
        path: &Path,
        version: &str,
        client: Arc<dyn PluginClient>,
        active_calls: Arc<AtomicUsize>,
    ) -> Result<Arc<LoadedPlugin>, PluginError> {
        let reg_bytes = client.call(METHOD_PLUGIN_REGISTER, b"{}")?;
        let reg_env: Envelope = serde_json::from_slice(&reg_bytes).map_err(|e| PluginError {
            code: "invalid_registration".to_string(),
            message: format!("decode registration envelope: {e}"),
            retryable: false,
            http_status: Some(500),
        })?;

        if !reg_env.ok {
            let err = reg_env.error.unwrap_or(PluginError {
                code: "registration_failed".to_string(),
                message: "plugin registration rejected".to_string(),
                retryable: false,
                http_status: Some(500),
            });
            return Err(err);
        }

        let reg_res = reg_env.result.unwrap_or_default();
        let schema_version = reg_res
            .get("schema_version")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(1);

        let metadata = reg_res
            .get("metadata")
            .cloned()
            .unwrap_or(serde_json::Value::Object(Default::default()));
        let capabilities = reg_res
            .get("capabilities")
            .cloned()
            .unwrap_or(serde_json::Value::Object(Default::default()));
        let name = metadata
            .get("Name")
            .or_else(|| metadata.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or(id)
            .to_string();

        let loaded_plugin = Arc::new(LoadedPlugin {
            id: id.to_string(),
            path: path.to_path_buf(),
            version: version.to_string(),
            name,
            schema_version,
            metadata,
            capabilities,
            client,
            active_calls,
        });

        {
            let mut loaded = self.inner.loaded.write().unwrap();
            loaded.insert(id.to_string(), loaded_plugin.clone());
            let mut versions = self.inner.active_versions.write().unwrap();
            versions.insert(id.to_string(), version.to_string());
        }

        Ok(loaded_plugin)
    }

    pub fn get_plugin(&self, id: &str) -> Option<Arc<LoadedPlugin>> {
        let loaded = self.inner.loaded.read().unwrap();
        loaded.get(id).cloned()
    }

    pub fn list_plugins(&self) -> Vec<Arc<LoadedPlugin>> {
        let loaded = self.inner.loaded.read().unwrap();
        loaded.values().cloned().collect()
    }
}

pub fn load_plugins_from_directory(host: &PluginHost, dir: &Path) {
    let ext = if cfg!(target_os = "macos") {
        ".dylib"
    } else if cfg!(target_os = "windows") {
        ".dll"
    } else {
        ".so"
    };

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) if name.ends_with(ext) => name,
            _ => continue,
        };

        let stem = &file_name[..file_name.len() - ext.len()];
        let (id, version) = if let Some((base, ver)) = stem.split_once("-v") {
            (base, ver)
        } else {
            (stem, "")
        };

        match host.load_plugin(id, &path, version) {
            Ok(p) => {
                host.clear_load_error(id);
                tracing::info!(plugin = %id, name = %p.name, path = %path.display(), "plugin loaded successfully");
            }
            Err(e) => {
                host.record_load_error(id, path.clone(), e.clone());
                tracing::warn!(plugin = %id, path = %path.display(), error = %e.message, "failed to load plugin");
            }
        }
    }
}
