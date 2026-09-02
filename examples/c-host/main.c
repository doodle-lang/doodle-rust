/* C host smoke test for the Doodle C ABI. Builds against the cbindgen-generated
 * doodle.h and the doodle-capi static library; see scripts/capi-smoke.sh. It links
 * the ABI (checking symbol/layout compatibility a real C compiler enforces that the
 * Rust-side tests cannot) and exercises the M7.1 core: version + ABI version, load →
 * drive → outcome, and a value-handle round-trip. */

#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "doodle.h"

/* Prints a message and returns 1 (the smoke test fails on the first problem). */
static int fail(const char *msg) {
    fprintf(stderr, "FAIL: %s\n", msg);
    return 1;
}

int main(void) {
    const char *version = doodle_version();
    if (version == NULL || strlen(version) == 0) {
        return fail("doodle_version() returned an empty string");
    }
    printf("doodle-capi version: %s\n", version);

    /* The runtime ABI version must match the header the host compiled against. */
    uint32_t abi = doodle_abi_version();
    uint32_t expected = ((uint32_t)DOODLE_ABI_VERSION_MAJOR << 16) | DOODLE_ABI_VERSION_MINOR;
    if (abi != expected) {
        return fail("doodle_abi_version() disagrees with the header macros");
    }

    /* Load a trivial program and drive it to completion. */
    const char *program = "let a = 1 + 2\n";
    DoodleInstance *inst = NULL;
    DoodleStatus status = doodle_load((const uint8_t *)program, strlen(program), NULL, &inst,
                                      NULL, 0, NULL);
    if (status != DoodleStatus_Ok || inst == NULL) {
        return fail("doodle_load() of a clean program failed");
    }

    DoodleOutcome outcome;
    status = doodle_drive(inst, DoodleDirective_RunToCompletion, &outcome);
    if (status != DoodleStatus_Ok || outcome.kind != DoodleOutcomeKind_Completed) {
        doodle_free(inst);
        return fail("the program did not complete");
    }

    /* A value-handle round-trip: make an int, read it back, release it. */
    DoodleHandle h = DOODLE_NULL_HANDLE;
    if (doodle_make_int(inst, 42, &h) != DoodleStatus_Ok) {
        doodle_free(inst);
        return fail("doodle_make_int() failed");
    }
    int64_t n = 0;
    if (doodle_as_int(inst, h, &n) != DoodleStatus_Ok || n != 42) {
        doodle_free(inst);
        return fail("doodle_as_int() did not round-trip 42");
    }
    if (doodle_release(inst, h) != DoodleStatus_Ok) {
        doodle_free(inst);
        return fail("doodle_release() failed");
    }

    doodle_free(inst);

    /* Register the `print` built-in and check its output crosses the ABI. */
    DoodleRegistry *registry = doodle_registry_new();
    if (registry == NULL
        || doodle_registry_add_builtin(registry, DoodleBuiltin_Print) != DoodleStatus_Ok) {
        return fail("doodle_registry_add_builtin(Print) failed");
    }
    const char *printer = "print(1 + 2)\n";
    DoodleInstance *pinst = NULL;
    status = doodle_load_with_registry((const uint8_t *)printer, strlen(printer), NULL, registry,
                                       &pinst, NULL, 0, NULL);
    if (status != DoodleStatus_Ok || pinst == NULL) {
        return fail("doodle_load_with_registry() failed");
    }
    status = doodle_drive(pinst, DoodleDirective_RunToCompletion, &outcome);
    if (status != DoodleStatus_Ok || outcome.kind != DoodleOutcomeKind_Completed) {
        doodle_free(pinst);
        return fail("the printing program did not complete");
    }
    /* Copy out the output; it must be "3\n". */
    char out[8];
    size_t out_len = 0;
    status = doodle_output(pinst, (uint8_t *)out, sizeof out, &out_len);
    if (status != DoodleStatus_Ok || out_len != 2 || out[0] != '3' || out[1] != '\n') {
        doodle_free(pinst);
        return fail("print output was not \"3\\n\"");
    }
    doodle_free(pinst);

    printf("c-host smoke: load/drive/handle + registry/print round-trip OK\n");
    return 0;
}
