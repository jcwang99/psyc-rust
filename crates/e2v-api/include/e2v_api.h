#ifndef E2V_API_H
#define E2V_API_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct e2v_sdk_t e2v_sdk_t;
typedef struct e2v_read_handle_t e2v_read_handle_t;
typedef struct e2v_snapshot_view_t e2v_snapshot_view_t;
typedef struct e2v_file_view_t e2v_file_view_t;
typedef struct e2v_error_t e2v_error_t;

typedef struct e2v_string_t {
    const char *ptr;
    size_t len;
} e2v_string_t;

typedef struct e2v_bytes_t {
    const uint8_t *ptr;
    size_t len;
} e2v_bytes_t;

typedef enum e2v_error_code_t {
    E2V_OK = 0,
    E2V_INVALID_ARGUMENT = 1,
    E2V_NOT_FOUND = 2,
    E2V_ALREADY_EXISTS = 3,
    E2V_PERMISSION_DENIED = 4,
    E2V_AUTHENTICATION_REQUIRED = 5,
    E2V_CONFLICT = 6,
    E2V_NEEDS_REBASE = 7,
    E2V_ROLLBACK_DETECTED = 8,
    E2V_UNSUPPORTED = 9,
    E2V_CORRUPT_STATE = 10,
    E2V_IO = 11,
    E2V_INTERNAL = 12,
    E2V_INTERNAL_PANIC = 255
} e2v_error_code_t;

e2v_error_code_t e2v_sdk_new(e2v_sdk_t **sdk_out, e2v_error_t **error_out);
void e2v_sdk_free(e2v_sdk_t *handle);
void e2v_read_handle_free(e2v_read_handle_t *handle);
void e2v_snapshot_view_free(e2v_snapshot_view_t *handle);
void e2v_file_view_free(e2v_file_view_t *handle);
void e2v_error_free(e2v_error_t *handle);
e2v_error_code_t e2v_error_code(e2v_error_t *handle);
e2v_error_code_t e2v_error_message(e2v_error_t *handle, e2v_string_t *message_out);
void e2v_string_free(e2v_string_t *value);
void e2v_bytes_free(e2v_bytes_t *value);

e2v_error_code_t e2v_init_repository_json(e2v_sdk_t *sdk, const char *repo_root, const char *password, const char *branch_name, e2v_string_t *json_out, e2v_error_t **error_out);
e2v_error_code_t e2v_open_repository_json(e2v_sdk_t *sdk, const char *repo_root, e2v_string_t *json_out, e2v_error_t **error_out);
e2v_error_code_t e2v_unlock_repository_json(e2v_sdk_t *sdk, const char *repo_root, const char *password, e2v_string_t *json_out, e2v_error_t **error_out);
e2v_error_code_t e2v_commit_repository_json(e2v_sdk_t *sdk, const char *repo_root, const char *message, e2v_string_t *json_out, e2v_error_t **error_out);
e2v_error_code_t e2v_list_snapshots_json(e2v_sdk_t *sdk, const char *repo_root, e2v_string_t *json_out, e2v_error_t **error_out);
e2v_error_code_t e2v_verify_snapshot(e2v_sdk_t *sdk, const char *repo_root, const char *snapshot_id, e2v_error_t **error_out);
e2v_error_code_t e2v_checkout_snapshot(e2v_sdk_t *sdk, const char *repo_root, const char *snapshot_id, const char *target_dir, e2v_error_t **error_out);
e2v_error_code_t e2v_change_password(e2v_sdk_t *sdk, const char *repo_root, const char *old_password, const char *new_password, e2v_error_t **error_out);
e2v_error_code_t e2v_create_branch_json(e2v_sdk_t *sdk, const char *repo_root, const char *branch_name, e2v_string_t *json_out, e2v_error_t **error_out);
e2v_error_code_t e2v_list_branches_json(e2v_sdk_t *sdk, const char *repo_root, e2v_string_t *json_out, e2v_error_t **error_out);
e2v_error_code_t e2v_checkout_branch_json(e2v_sdk_t *sdk, const char *repo_root, const char *branch_name, e2v_string_t *json_out, e2v_error_t **error_out);
e2v_error_code_t e2v_delete_branch(e2v_sdk_t *sdk, const char *repo_root, const char *branch_name, e2v_error_t **error_out);

e2v_error_code_t e2v_open_read_handle(e2v_sdk_t *sdk, const char *repo_root, e2v_read_handle_t **handle_out, e2v_error_t **error_out);
e2v_error_code_t e2v_open_snapshot(e2v_read_handle_t *read_handle, const char *snapshot_id, e2v_snapshot_view_t **handle_out, e2v_error_t **error_out);
e2v_error_code_t e2v_resolve_branch(e2v_read_handle_t *read_handle, const char *branch_token, e2v_snapshot_view_t **handle_out, e2v_error_t **error_out);
e2v_error_code_t e2v_open_file(e2v_read_handle_t *read_handle, e2v_snapshot_view_t *snapshot, const char *path, e2v_file_view_t **handle_out, e2v_error_t **error_out);
e2v_error_code_t e2v_read_dir_json(e2v_read_handle_t *read_handle, e2v_snapshot_view_t *snapshot, const char *path, e2v_string_t *json_out, e2v_error_t **error_out);
e2v_error_code_t e2v_read_range(e2v_read_handle_t *read_handle, e2v_file_view_t *file, size_t offset, size_t length, e2v_bytes_t *bytes_out, e2v_error_t **error_out);

e2v_error_code_t e2v_parse_remote_spec_json(const char *spec, e2v_string_t *json_out, e2v_error_t **error_out);
e2v_error_code_t e2v_add_remote_json(e2v_sdk_t *sdk, const char *repo_root, const char *name, const char *spec, e2v_string_t *json_out, e2v_error_t **error_out);
e2v_error_code_t e2v_load_default_remote_json(e2v_sdk_t *sdk, const char *repo_root, e2v_string_t *json_out, e2v_error_t **error_out);
e2v_error_code_t e2v_push_default_remote_json(e2v_sdk_t *sdk, const char *repo_root, const char *branch_token, const char *operation_id, e2v_string_t *json_out, e2v_error_t **error_out);
e2v_error_code_t e2v_fetch_default_remote_json(e2v_sdk_t *sdk, const char *repo_root, const char *branch_token, const char *password, e2v_string_t *json_out, e2v_error_t **error_out);
e2v_error_code_t e2v_clone_remote_json(e2v_sdk_t *sdk, const char *remote_spec, const char *target_repo_root, const char *password, const char *branch_token, e2v_string_t *json_out, e2v_error_t **error_out);
e2v_error_code_t e2v_verify_default_remote_json(e2v_sdk_t *sdk, const char *repo_root, uint8_t sample_percent, e2v_string_t *json_out, e2v_error_t **error_out);
e2v_error_code_t e2v_repair_default_remote_json(e2v_sdk_t *sdk, const char *repo_root, e2v_string_t *json_out, e2v_error_t **error_out);
e2v_error_code_t e2v_force_accept_default_remote_rollback_json(e2v_sdk_t *sdk, const char *repo_root, const char *password, e2v_string_t *json_out, e2v_error_t **error_out);
e2v_error_code_t e2v_gc_default_remote_dry_run_json(e2v_sdk_t *sdk, const char *repo_root, e2v_string_t *json_out, e2v_error_t **error_out);
e2v_error_code_t e2v_gc_default_remote_execute_json(e2v_sdk_t *sdk, const char *repo_root, uint64_t grace_period_days, bool allow_single_writer_maintenance_window, e2v_string_t *json_out, e2v_error_t **error_out);

e2v_error_code_t e2v_share_list_json(e2v_sdk_t *sdk, const char *repo_root, e2v_string_t *json_out, e2v_error_t **error_out);
e2v_error_code_t e2v_share_invite_member_json(e2v_sdk_t *sdk, const char *repo_root, const char *display_name, e2v_string_t *json_out, e2v_error_t **error_out);
e2v_error_code_t e2v_share_accept_member_json(e2v_sdk_t *sdk, const char *repo_root, const uint8_t *invite_bytes, size_t invite_len, const char *local_device_label, e2v_string_t *json_out, e2v_error_t **error_out);
e2v_error_code_t e2v_share_invite_device_json(e2v_sdk_t *sdk, const char *repo_root, const char *actor_id, const char *device_label, e2v_string_t *json_out, e2v_error_t **error_out);
e2v_error_code_t e2v_share_accept_device_json(e2v_sdk_t *sdk, const char *repo_root, const uint8_t *invite_bytes, size_t invite_len, const char *local_device_label, e2v_string_t *json_out, e2v_error_t **error_out);
e2v_error_code_t e2v_share_revoke_device(e2v_sdk_t *sdk, const char *repo_root, const char *device_id, const char *password, e2v_error_t **error_out);
e2v_error_code_t e2v_share_revoke_member(e2v_sdk_t *sdk, const char *repo_root, const char *actor_id, const char *password, e2v_error_t **error_out);

e2v_error_code_t e2v_test_only_force_panic(e2v_error_t **error_out);

#ifdef __cplusplus
}
#endif

#endif
