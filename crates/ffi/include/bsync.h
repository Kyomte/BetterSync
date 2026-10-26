/* BetterSync daemon, embedded (crates/ffi). Strings in are UTF-8 and
 * NUL-terminated; strings returned must be freed with bsync_free_string.
 * Functions returning a string return "error: ..." on failure. */
#ifndef BSYNC_H
#define BSYNC_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Start the daemon (once per process) with its config in config_dir.
 * Returns the status page URL, with the API token in its fragment. */
char *bsync_start(const char *config_dir);

/* Start for a phone app: folders that paired devices share appear under
 * folders_root/<folder id>. If app_discovery is non-zero the app reports
 * LAN devices with bsync_discovered instead of the daemon's own mDNS. */
char *bsync_start_mobile(const char *config_dir, const char *folders_root, int32_t app_discovery);

/* A device the app found on the LAN: device_id from its "id" TXT record,
 * addrs as comma-separated "ip:port". Returns 0 on success. */
int32_t bsync_discovered(const char *device_id, const char *addrs);

/* Reconnect to paired devices now (back in the foreground, network change). */
void bsync_network_changed(void);

/* Status as JSON, the same document as GET /api/status. */
char *bsync_status_json(const char *config_dir);

void bsync_stop(void);

void bsync_free_string(char *s);

#ifdef __cplusplus
}
#endif

#endif /* BSYNC_H */
