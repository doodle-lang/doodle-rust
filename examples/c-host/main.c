/* Minimal C host smoke test for the Doodle C ABI (M0.6). Builds against the
 * cbindgen-generated doodle.h and the doodle-capi library; see
 * scripts/capi-smoke.sh. */

#include <stdio.h>
#include <string.h>

#include "doodle.h"

int main(void) {
    const char *version = doodle_version();
    if (version == NULL || strlen(version) == 0) {
        fprintf(stderr, "FAIL: doodle_version() returned an empty string\n");
        return 1;
    }
    printf("doodle-capi version: %s\n", version);
    return 0;
}
