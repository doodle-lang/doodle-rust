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

/* A host foreign `to greet(who="world", body)` (M7.2b): reads its one bound argument as a
 * handle and invokes the block with it, then frees the arg handle. The callback runs inside the
 * drive and calls back in through the ctx — never reconstructing a DoodleInstance. */
static DoodleStatus greet_cb(DoodleCallCtx *ctx, void *user_data) {
    (void)user_data;
    DoodleHandle who = DOODLE_NULL_HANDLE;
    DoodleStatus status = doodle_call_arg(ctx, 0, &who);
    if (status != DoodleStatus_Ok) {
        return status;
    }
    DoodleBlockOutcome outcome = DoodleBlockOutcome_Completed;
    status = doodle_call_block(ctx, &who, 1, &outcome);
    doodle_call_release(ctx, who);
    return status;
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

    /* Register a host foreign function and drive it: greet() defaults `who` to "world" (a
     * string default) and invokes its block with it — the M7.2b control-inversion accept path,
     * exercised from real C. */
    DoodleRegistry *freg = doodle_registry_new();
    if (freg == NULL
        || doodle_registry_add_builtin(freg, DoodleBuiltin_Print) != DoodleStatus_Ok) {
        return fail("registry for the foreign-fn smoke failed");
    }
    DoodleForeignDesc *desc =
        doodle_foreign_desc_new((const uint8_t *)"greet", 5, DoodleBodyKind_Proc);
    if (desc == NULL
        || doodle_foreign_desc_default_string(desc, (const uint8_t *)"who", 3,
                                              (const uint8_t *)"world", 5) != DoodleStatus_Ok
        || doodle_foreign_desc_block_param(desc, (const uint8_t *)"body", 4) != DoodleStatus_Ok
        || doodle_foreign_desc_set_callback(desc, greet_cb, NULL) != DoodleStatus_Ok
        || doodle_registry_add_foreign(freg, desc) != DoodleStatus_Ok) {
        return fail("building/registering the greet foreign function failed");
    }
    const char *greeter = "greet() do (who)\nprint(who)\nend\n";
    DoodleInstance *ginst = NULL;
    status = doodle_load_with_registry((const uint8_t *)greeter, strlen(greeter), NULL, freg,
                                       &ginst, NULL, 0, NULL);
    if (status != DoodleStatus_Ok || ginst == NULL) {
        return fail("doodle_load_with_registry() for greet failed");
    }
    status = doodle_drive(ginst, DoodleDirective_RunToCompletion, &outcome);
    if (status != DoodleStatus_Ok || outcome.kind != DoodleOutcomeKind_Completed) {
        doodle_free(ginst);
        return fail("the greet program did not complete");
    }
    char gout[8];
    size_t gout_len = 0;
    status = doodle_output(ginst, (uint8_t *)gout, sizeof gout, &gout_len);
    if (status != DoodleStatus_Ok || gout_len != 6 || memcmp(gout, "world\n", 6) != 0) {
        doodle_free(ginst);
        return fail("greet output was not \"world\\n\"");
    }
    doodle_free(ginst);

    /* Observation surface (M7.3): Step to a pause, walk the stack, and read the current
     * position — the debugger's pull view, exercised from real C. */
    const char *stepper = "let a = 1\nlet b = 2\n";
    DoodleInstance *sinst = NULL;
    status = doodle_load((const uint8_t *)stepper, strlen(stepper), NULL, &sinst, NULL, 0, NULL);
    if (status != DoodleStatus_Ok || sinst == NULL) {
        return fail("doodle_load() for the observation walk failed");
    }
    status = doodle_drive(sinst, DoodleDirective_Step, &outcome);
    if (status != DoodleStatus_Ok || outcome.kind != DoodleOutcomeKind_Paused) {
        doodle_free(sinst);
        return fail("the Step drive did not pause");
    }
    uint32_t frame_count = 0, generation = 0;
    if (doodle_stack_frame_count(sinst, &frame_count, &generation) != DoodleStatus_Ok
        || frame_count < 1) {
        doodle_free(sinst);
        return fail("stack walk reported no frames");
    }
    DoodleFrame frame;
    if (doodle_frame_at(sinst, generation, frame_count - 1, &frame) != DoodleStatus_Ok
        || frame.has_callable) {
        doodle_free(sinst);
        return fail("the outermost frame should be the module top (no callable)");
    }
    /* A stale generation is a benign, distinct error (not a contract violation). */
    if (doodle_frame_at(sinst, generation + 1, 0, &frame) != DoodleStatus_ErrStale) {
        doodle_free(sinst);
        return fail("a stale generation should report DoodleStatus_ErrStale");
    }
    DoodlePosition position;
    bool has_position = false;
    if (doodle_current_position(sinst, &position, &has_position) != DoodleStatus_Ok
        || !has_position) {
        doodle_free(sinst);
        return fail("a paused instance should have a current position");
    }
    doodle_free(sinst);

    printf("c-host smoke: load/drive/handle + registry/print + foreign-fn + observation OK\n");
    return 0;
}
