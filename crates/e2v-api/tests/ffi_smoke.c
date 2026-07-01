#include "e2v_api.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <windows.h>

static void fail_with_error(e2v_error_t *error) {
    e2v_string_t message = {0};
    if (error != NULL) {
        e2v_error_message(error, &message);
    }
    if (message.ptr != NULL) {
        fprintf(stderr, "ffi smoke failed: %.*s\n", (int)message.len, message.ptr);
        e2v_string_free(&message);
    } else {
        fprintf(stderr, "ffi smoke failed without error message\n");
    }
    if (error != NULL) {
        e2v_error_free(error);
    }
    exit(1);
}

int main(void) {
    e2v_sdk_t *sdk = NULL;
    e2v_error_t *error = NULL;
    e2v_string_t json = {0};
    e2v_read_handle_t *read_handle = NULL;
    e2v_snapshot_view_t *snapshot = NULL;
    e2v_file_view_t *file = NULL;
    e2v_bytes_t bytes = {0};
    char repo_root[MAX_PATH];
    DWORD temp_len = GetTempPathA(MAX_PATH, repo_root);
    if (temp_len == 0 || temp_len >= MAX_PATH) {
        fprintf(stderr, "failed to get temp path for ffi smoke\n");
        return 1;
    }
    {
        char unique_dir[64];
        snprintf(unique_dir, sizeof(unique_dir), "e2v-ffi-smoke-%lu", GetCurrentProcessId());
        if (strlen(repo_root) + strlen(unique_dir) + 2 >= MAX_PATH) {
            fprintf(stderr, "temp path too long for ffi smoke\n");
            return 1;
        }
        strcat(repo_root, unique_dir);
    }
    CreateDirectoryA(repo_root, NULL);

    if (e2v_sdk_new(&sdk, &error) != E2V_OK) {
        fail_with_error(error);
    }

    if (e2v_init_repository_json(
            sdk,
            repo_root,
            "correct horse battery staple",
            "main",
            &json,
            &error) != E2V_OK) {
        fail_with_error(error);
    }
    e2v_string_free(&json);

    {
        char tracked_path[MAX_PATH];
        snprintf(tracked_path, sizeof(tracked_path), "%s\\hello.txt", repo_root);
        FILE *tracked = fopen(tracked_path, "wb");
        if (tracked == NULL) {
            fprintf(stderr, "failed to open tracked file for ffi smoke\n");
            return 1;
        }
        fwrite("hello ffi smoke", 1, strlen("hello ffi smoke"), tracked);
        fclose(tracked);
    }

    if (e2v_commit_repository_json(sdk, repo_root, "seed", &json, &error) != E2V_OK) {
        fail_with_error(error);
    }
    e2v_string_free(&json);

    if (e2v_open_read_handle(sdk, repo_root, &read_handle, &error) != E2V_OK) {
        fail_with_error(error);
    }

    if (e2v_open_repository_json(sdk, repo_root, &json, &error) != E2V_OK) {
        fail_with_error(error);
    }
    if (json.ptr == NULL || strstr(json.ptr, "\"token_hex\"") == NULL) {
        fprintf(stderr, "repository json missing branch token\n");
        return 1;
    }
    {
        char branch_token[256] = {0};
        const char *token_field = strstr(json.ptr, "\"token_hex\":\"");
        const char *token_start;
        const char *token_end;
        size_t token_len;
        if (token_field == NULL) {
            fprintf(stderr, "repository json missing token field\n");
            return 1;
        }
        token_start = token_field + strlen("\"token_hex\":\"");
        token_end = strchr(token_start, '"');
        if (token_end == NULL) {
            fprintf(stderr, "repository json malformed token field\n");
            return 1;
        }
        token_len = (size_t)(token_end - token_start);
        if (token_len >= sizeof(branch_token)) {
            fprintf(stderr, "branch token too long for smoke parser\n");
            return 1;
        }
        memcpy(branch_token, token_start, token_len);
        branch_token[token_len] = '\0';
        e2v_string_free(&json);

        if (e2v_resolve_branch(read_handle, branch_token, &snapshot, &error) != E2V_OK) {
            fail_with_error(error);
        }
    }

    if (e2v_open_file(read_handle, snapshot, "hello.txt", &file, &error) != E2V_OK) {
        fail_with_error(error);
    }

    if (e2v_read_range(read_handle, file, 0, 64, &bytes, &error) != E2V_OK) {
        fail_with_error(error);
    }

    if (bytes.ptr == NULL || bytes.len != strlen("hello ffi smoke") ||
        memcmp(bytes.ptr, "hello ffi smoke", bytes.len) != 0) {
        fprintf(stderr, "ffi smoke read mismatch\n");
        return 1;
    }

    e2v_bytes_free(&bytes);
    e2v_file_view_free(file);
    e2v_snapshot_view_free(snapshot);
    e2v_read_handle_free(read_handle);
    e2v_sdk_free(sdk);
    return 0;
}
