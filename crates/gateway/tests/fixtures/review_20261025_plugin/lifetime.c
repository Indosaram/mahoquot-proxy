/* Dedicated, single-instance ABI v1 fixture. Built only by the integration test.
 * Gates live in the host callback, not in timing loops or native global locks.
 * These declarations deliberately mirror plugins/abi.rs's repr(C) tables. */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

typedef struct { void *ptr; size_t len; } Buffer;
typedef int32_t (*HostCall)(void *, const char *, const uint8_t *, size_t, Buffer *);
typedef void (*FreeBuffer)(void *, size_t);
typedef struct {
    uint32_t abi_version;
    void *host_ctx;
    HostCall call;
    FreeBuffer free_buffer;
} HostApi;
typedef struct {
    uint32_t abi_version;
    int32_t (*call)(const char *, const uint8_t *, size_t, Buffer *);
    FreeBuffer free_buffer;
    void (*shutdown)(void);
} PluginApi;

static const HostApi *host;
static int unload_fd = -1;
static void *active_response;
static void *quiesce_response;

static int callback(const char *method) {
    Buffer response = {0};
    int rc = host->call(host->host_ctx, method, (const uint8_t *)"native", 6, &response);
    int valid = rc == 0 && response.ptr && response.len == 3 &&
                memcmp(response.ptr, "ack", 3) == 0;
    if (response.ptr) host->free_buffer(response.ptr, response.len);
    return valid;
}

static int32_t reply(Buffer *out, const char *bytes) {
    out->len = strlen(bytes);
    out->ptr = malloc(out->len);
    if (!out->ptr) return 1;
    memcpy(out->ptr, bytes, out->len);
    return 0;
}

static int32_t call(const char *method, const uint8_t *request, size_t len, Buffer *out) {
    if (strcmp(method, "plugin.register") == 0)
        return reply(out, "{\"ok\":true,\"result\":{\"schema_version\":6,\"metadata\":{\"Name\":\"native-lifetime\"}}}");
    if (strcmp(method, "fixture.ping") == 0)
        return reply(out, "pong");
    if (strcmp(method, "fixture.configure") == 0) {
        char fd_text[32];
        if (len == 0 || len >= sizeof(fd_text) || unload_fd != -1) return 1;
        memcpy(fd_text, request, len);
        fd_text[len] = 0;
        unload_fd = dup(atoi(fd_text));
        if (unload_fd < 0) return 1;
        return reply(out, "configured");
    }
    if (strcmp(method, "fixture.active") == 0) {
        if (!callback("native.call.entered")) return 1;
        /* Runs AFTER unload was rejected while the callback was on-stack. */
        if (!callback("native.call.resumed")) return 1;
        int32_t rc = reply(out, "native-owned-response");
        active_response = out->ptr;
        return rc;
    }
    if (strcmp(method, "plugin.quiesce") == 0) {
        if (!callback("native.quiesce.entered")) return 1;
        if (!callback("native.quiesce.resumed")) return 1;
        int32_t rc = reply(out, "{\"ok\":true,\"result\":{}}");
        quiesce_response = out->ptr;
        return rc;
    }
    return 1;
}

static void free_buffer(void *ptr, size_t len) {
    (void)len;
    if (ptr == active_response) {
        if (!callback("native.free.entered")) abort();
        /* Host context/table and native code must survive this second gate. */
        if (!callback("native.free.resumed")) abort();
        active_response = NULL;
    } else if (ptr == quiesce_response) {
        if (!callback("native.quiesce.free")) abort();
        quiesce_response = NULL;
    }
    free(ptr);
}

static void shutdown_plugin(void) {
    if (!callback("native.shutdown")) abort();
}

__attribute__((destructor)) static void unloaded(void) {
    if (unload_fd >= 0) {
        /* Independent observation of dlclose, without retaining another handle. */
        if (write(unload_fd, "D", 1) != 1) abort();
        close(unload_fd);
        unload_fd = -1;
    }
}

int32_t cliproxy_plugin_init(const HostApi *input, PluginApi *output) {
    if (!input || input->abi_version != 1 || !input->call || !input->free_buffer)
        return 1;
    host = input; /* Retain the actual host table pointer to exercise its lifetime. */
    *output = (PluginApi){1, call, free_buffer, shutdown_plugin};
    return 0;
}
