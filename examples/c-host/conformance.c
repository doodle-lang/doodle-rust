/* conformance.c — the M7.5e example C conformance host.
 *
 * Embeds the Doodle engine through the C ABI, registers the portable conformance manifest (M7.5b:
 * print/length/each/encode/decode + the read_line/time/random capabilities + the S-42 test_greet
 * foreign function), drives ONE fixture, and emits its transcript (M7.5d transcript v1) to stdout.
 * The Rust orchestrator (`conformance-runner --c-host`) discovers/parses fixtures, runs this host
 * per fixture, finalizes positions, and compares to the committed canonical `.transcript`.
 *
 * Positions are emitted as `<canonical-module>#<byte-offset>` — the ABI hands out byte spans, not
 * line/columns, and there is no NFC-source accessor, so the orchestrator (which has doodle-core's
 * normalize + line index) maps offset -> line:col. See scripts/capi-conformance.sh.
 *
 * Invocation: `conformance <fixture.doodle> [<modules_dir>]`, with the job on stdin:
 *     mode run
 *     input <capability> <canonical-response>     (e.g. `input read_line "hi"`, `input time 100`)
 * Only `mode run` is handled here; `mode drive` is the M7.5e-2 extension. */

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "doodle.h"

static void die(const char *msg) {
    fprintf(stderr, "c-host: %s\n", msg);
    exit(2);
}

/* Reads an entire stream into a NUL-terminated, malloc'd buffer (`*out_len` excludes the NUL). */
static uint8_t *slurp(FILE *f, size_t *out_len) {
    size_t cap = 4096, len = 0;
    uint8_t *buf = malloc(cap);
    if (!buf) die("out of memory");
    for (;;) {
        if (len + 1 >= cap) {
            cap *= 2;
            buf = realloc(buf, cap);
            if (!buf) die("out of memory");
        }
        size_t n = fread(buf + len, 1, cap - len - 1, f);
        len += n;
        if (n == 0) break;
    }
    buf[len] = 0;
    *out_len = len;
    return buf;
}

static uint8_t *slurp_path(const char *path, size_t *out_len) {
    FILE *f = fopen(path, "rb");
    if (!f) die("cannot open fixture file");
    uint8_t *buf = slurp(f, out_len);
    fclose(f);
    return buf;
}

/* --- transcript escaping (must match the native emitter, transcript.rs) --- */

/* Writes `bytes` with control bytes / '\' / '"' / DEL as \xNN; every other byte verbatim. */
static void put_escaped(const uint8_t *bytes, size_t len, int escape_quote) {
    for (size_t i = 0; i < len; i++) {
        uint8_t b = bytes[i];
        if (b < 0x20 || b == '\\' || b == 0x7f || (escape_quote && b == '"')) {
            printf("\\x%02x", b);
        } else {
            putchar(b);
        }
    }
}

/* --- the scripted run-mode job (stdin) --- */

#define MAX_INPUTS 64
struct Input {
    char cap[32];      /* capability name */
    char *response;    /* canonical response text (echoed for `res:`, parsed to materialize) */
    int used;          /* drawn from the FIFO already */
};
static struct Input g_inputs[MAX_INPUTS];
static int g_input_count = 0;

/* The drive-mode job (M7.5e-2): debug setup applied once, then the ordered driving actions whose
 * `step:`/`stop:` transcript this host emits. */
#define MAX_BREAKS 16
#define MAX_ACTIONS 64
static int g_drive = 0; /* 0 = run, 1 = drive */
static uint32_t g_break_lines[MAX_BREAKS];
static int g_break_count = 0;
static int g_raise_trap = 0;
static int g_subexpr = 0;
static char *g_actions[MAX_ACTIONS]; /* each is a `step:` label: `continue`, `into`, `resolve "x"`… */
static int g_action_count = 0;

/* Parses the stdin job: a `mode` line, then either run-mode `input <cap> <response>` lines or
 * drive-mode `break`/`raise-trap`/`obs` setup + `do <action>` lines. */
static void read_job(void) {
    size_t len = 0;
    char *job = (char *)slurp(stdin, &len);
    char *line = strtok(job, "\n");
    int seen_mode = 0;
    while (line) {
        if (strncmp(line, "mode ", 5) == 0) {
            if (strcmp(line + 5, "drive") == 0) g_drive = 1;
            else if (strcmp(line + 5, "run") != 0) die("unknown mode");
            seen_mode = 1;
        } else if (strncmp(line, "input ", 6) == 0) {
            if (g_input_count >= MAX_INPUTS) die("too many inputs");
            char *rest = line + 6;
            char *sp = strchr(rest, ' ');
            if (!sp) die("malformed input line");
            *sp = 0;
            struct Input *in = &g_inputs[g_input_count++];
            snprintf(in->cap, sizeof in->cap, "%s", rest);
            in->response = strdup(sp + 1);
            in->used = 0;
        } else if (strncmp(line, "break ", 6) == 0) {
            /* `break <canonical> <line>` — this host's fixtures all break in the entry module. */
            char *sp = strrchr(line, ' ');
            if (g_break_count < MAX_BREAKS) g_break_lines[g_break_count++] = (uint32_t)atoi(sp + 1);
        } else if (strcmp(line, "raise-trap on") == 0) {
            g_raise_trap = 1;
        } else if (strcmp(line, "obs subexpr") == 0) {
            g_subexpr = 1;
        } else if (strncmp(line, "do ", 3) == 0) {
            if (g_action_count < MAX_ACTIONS) g_actions[g_action_count++] = strdup(line + 3);
        }
        line = strtok(NULL, "\n");
    }
    if (!seen_mode) die("job did not declare a `mode`");
    /* `job` is intentionally leaked: the process is one-shot. */
}

/* The next unused scripted response for `cap` (FIFO), or NULL. */
static struct Input *next_input(const char *cap) {
    for (int i = 0; i < g_input_count; i++) {
        if (!g_inputs[i].used && strcmp(g_inputs[i].cap, cap) == 0) {
            g_inputs[i].used = 1;
            return &g_inputs[i];
        }
    }
    return NULL;
}

/* --- value literals: unescape a `"…"` payload (reverse of put_escaped) into raw bytes --- */

static size_t unescape(const char *src, size_t len, uint8_t *out) {
    size_t o = 0;
    for (size_t i = 0; i < len;) {
        if (src[i] == '\\' && i + 3 < len + 1 && src[i + 1] == 'x') {
            int hi = src[i + 2], lo = src[i + 3];
            int v = ((hi <= '9' ? hi - '0' : (hi | 0x20) - 'a' + 10) << 4)
                    | (lo <= '9' ? lo - '0' : (lo | 0x20) - 'a' + 10);
            out[o++] = (uint8_t)v;
            i += 4;
        } else {
            out[o++] = (uint8_t)src[i++];
        }
    }
    return o;
}

/* Materializes a canonical response (`"str"` / int / float / true|false|nil) into a handle. */
static DoodleHandle make_value(DoodleInstance *inst, const char *text) {
    DoodleHandle h = DOODLE_NULL_HANDLE;
    size_t tlen = strlen(text);
    if (text[0] == '"' && tlen >= 2) {
        uint8_t *raw = malloc(tlen);
        size_t n = unescape(text + 1, tlen - 2, raw);
        if (doodle_make_string(inst, raw, n, &h) != DoodleStatus_Ok) die("make_string failed");
        free(raw);
    } else if (strcmp(text, "true") == 0 || strcmp(text, "false") == 0) {
        doodle_make_bool(inst, text[0] == 't', &h);
    } else if (strcmp(text, "nil") == 0) {
        doodle_make_nil(inst, &h);
    } else if (strchr(text, '.')) {
        doodle_make_float(inst, strtod(text, NULL), &h);
    } else {
        doodle_make_int(inst, strtoll(text, NULL, 10), &h);
    }
    return h;
}

/* --- the S-42 test_greet foreign function (default punct="!", block body) --- */

static DoodleStatus test_greet_cb(DoodleCallCtx *ctx, void *user_data) {
    (void)user_data;
    DoodleHandle name = DOODLE_NULL_HANDLE, punct = DOODLE_NULL_HANDLE;
    if (doodle_call_arg(ctx, 0, &name) != DoodleStatus_Ok
        || doodle_call_arg(ctx, 1, &punct) != DoodleStatus_Ok) {
        return DoodleStatus_ErrContract;
    }
    uint8_t nb[256], pb[64];
    size_t nlen = 0, plen = 0;
    doodle_call_string_bytes(ctx, name, nb, sizeof nb, &nlen);
    doodle_call_string_bytes(ctx, punct, pb, sizeof pb, &plen);
    DoodleBlockOutcome outcome = DoodleBlockOutcome_Completed;
    DoodleStatus status = doodle_call_block(ctx, &name, 1, &outcome);
    doodle_call_release(ctx, name);
    doodle_call_release(ctx, punct);
    if (status != DoodleStatus_Ok) return status;
    if (outcome != DoodleBlockOutcome_Completed) return DoodleStatus_Ok; /* non-local exit */
    uint8_t result[320];
    size_t rlen = nlen < sizeof result ? nlen : 0;
    memcpy(result, nb, rlen);
    if (rlen + plen <= sizeof result) {
        memcpy(result + rlen, pb, plen);
        rlen += plen;
    }
    DoodleHandle out = DOODLE_NULL_HANDLE;
    if (doodle_call_make_string(ctx, result, rlen, &out) != DoodleStatus_Ok) {
        return DoodleStatus_ErrContract;
    }
    return doodle_call_set_result(ctx, out);
}

/* Registers the portable manifest in order (registration index = capability id / replay identity). */
static DoodleRegistry *build_registry(void) {
    DoodleRegistry *reg = doodle_registry_new();
    DoodleBuiltin order[] = {DoodleBuiltin_Print,   DoodleBuiltin_Length, DoodleBuiltin_Each,
                             DoodleBuiltin_Encode,  DoodleBuiltin_Decode, DoodleBuiltin_ReadLine,
                             DoodleBuiltin_Time,    DoodleBuiltin_Random};
    for (size_t i = 0; i < sizeof order / sizeof order[0]; i++) {
        if (doodle_registry_add_builtin(reg, order[i]) != DoodleStatus_Ok) die("add_builtin failed");
    }
    DoodleForeignDesc *desc =
        doodle_foreign_desc_new((const uint8_t *)"test_greet", 10, DoodleBodyKind_Func);
    if (!desc
        || doodle_foreign_desc_param(desc, (const uint8_t *)"name", 4) != DoodleStatus_Ok
        || doodle_foreign_desc_default_string(desc, (const uint8_t *)"punct", 5,
                                              (const uint8_t *)"!", 1) != DoodleStatus_Ok
        || doodle_foreign_desc_block_param(desc, (const uint8_t *)"body", 4) != DoodleStatus_Ok
        || doodle_foreign_desc_set_callback(desc, test_greet_cb, NULL) != DoodleStatus_Ok
        || doodle_registry_add_foreign(reg, desc) != DoodleStatus_Ok) {
        die("registering test_greet failed");
    }
    return reg;
}

/* capability id -> name (registration order above): read_line=5, time=6, random=7. */
static const char *cap_name(uint32_t id) {
    switch (id) {
        case 5: return "read_line";
        case 6: return "time";
        case 7: return "random";
        default: return NULL;
    }
}

static const char *fault_name(DoodleFault f) {
    switch (f) {
        case DoodleFault_LimitStepBudget: return "step-budget";
        case DoodleFault_LimitHeap: return "heap";
        case DoodleFault_LimitStackDepth: return "stack-depth";
        case DoodleFault_LimitTailHistory: return "tail-history";
        case DoodleFault_LimitOpResult: return "op-result";
        case DoodleFault_Cancelled: return "cancelled";
        case DoodleFault_NestedSuspend: return "nested-suspend";
        default: return "internal";
    }
}

/* Emits `<canonical-module>#<offset>` for a position (the orchestrator maps offset -> line:col). */
static void put_position(DoodleInstance *inst, uint32_t module_token, uint32_t offset) {
    char buf[256];
    size_t len = 0;
    if (doodle_module_canonical_id(inst, module_token, (uint8_t *)buf, sizeof buf, &len)
            != DoodleStatus_Ok
        || len > sizeof buf) {
        die("module_canonical_id failed");
    }
    printf("%.*s#%u", (int)len, buf, offset);
}

/* Reads the whole output and, if it grew past `*last`, emits the new bytes as one coalesced `out:`. */
static void emit_output(DoodleInstance *inst, uint8_t **buf, size_t *cap, size_t *last) {
    size_t needed = 0;
    doodle_output(inst, *buf, *cap, &needed);
    if (needed > *cap) {
        *buf = realloc(*buf, needed);
        *cap = needed;
        doodle_output(inst, *buf, *cap, &needed);
    }
    if (needed > *last) {
        printf("out: ");
        put_escaped(*buf + *last, needed - *last, 0);
        putchar('\n');
        *last = needed;
    }
}

/* Resolves a parked import to its sibling module file `<modules_dir>/<path>.doodle`, else NotFound. */
static void resolve_import(DoodleInstance *inst, const char *modules_dir, uint32_t segments,
                           DoodleOutcome *oc) {
    char joined[512];
    size_t jl = 0;
    for (uint32_t i = 0; i < segments; i++) {
        char seg[128];
        size_t sl = 0;
        doodle_import_path_segment(inst, i, (uint8_t *)seg, sizeof seg, &sl);
        if (i) joined[jl++] = '/';
        memcpy(joined + jl, seg, sl);
        jl += sl;
    }
    joined[jl] = 0;
    if (modules_dir[0]) {
        char path[700];
        snprintf(path, sizeof path, "%s/%s.doodle", modules_dir, joined);
        FILE *f = fopen(path, "rb");
        if (f) {
            size_t slen = 0;
            uint8_t *src = slurp(f, &slen);
            fclose(f);
            doodle_resolve_import(inst, src, slen, (const uint8_t *)joined, jl, oc);
            free(src);
            return;
        }
    }
    doodle_resolve_import_not_found(inst, oc);
}

/* --- drive mode (M7.5e-2): the E§8 debug surface, driven per the fixture's script --- */

static const char *reason_name(DoodlePauseReason r) {
    switch (r) {
        case DoodlePauseReason_Step: return "step";
        case DoodlePauseReason_Breakpoint: return "breakpoint";
        case DoodlePauseReason_RaiseTrap: return "raise-trap";
        case DoodlePauseReason_HostPause: return "host-pause";
        default: return "slice-end";
    }
}

/* Applies the drive setup once (breakpoints in the entry module, raise-trap, observation mode). */
static void drive_setup(DoodleInstance *inst) {
    for (int i = 0; i < g_break_count; i++) {
        uint32_t id = 0;
        doodle_set_breakpoint(inst, (const uint8_t *)"main", 4, g_break_lines[i], &id);
    }
    if (g_raise_trap) doodle_set_raise_trapping(inst, true);
    if (g_subexpr) doodle_set_observation_mode(inst, DoodleObservationMode_Subexpression);
}

/* Drives one action (its `step:` label): a directive, or a capability resolve. */
static void drive_action(DoodleInstance *inst, const char *label, DoodleOutcome *oc) {
    if (strcmp(label, "run") == 0) {
        doodle_drive(inst, DoodleDirective_RunToCompletion, oc);
    } else if (strcmp(label, "continue") == 0) {
        doodle_drive(inst, DoodleDirective_Continue, oc);
    } else if (strcmp(label, "step") == 0) {
        doodle_drive(inst, DoodleDirective_Step, oc);
    } else if (strcmp(label, "into") == 0) {
        doodle_drive(inst, DoodleDirective_StepInto, oc);
    } else if (strcmp(label, "over") == 0) {
        doodle_drive(inst, DoodleDirective_StepOver, oc);
    } else if (strcmp(label, "out") == 0) {
        doodle_drive(inst, DoodleDirective_StepOut, oc);
    } else if (strncmp(label, "resolve-raise ", 14) == 0) {
        doodle_resolve_raise(inst, make_value(inst, label + 14), oc);
    } else if (strncmp(label, "resolve ", 8) == 0) {
        doodle_resolve(inst, make_value(inst, label + 8), oc);
    } else {
        die("unknown drive action");
    }
}

/* Emits the `stop:` line for a drive outcome (positions as <module>#<offset>). */
static void emit_stop(DoodleInstance *inst, DoodleOutcome *oc) {
    if (oc->kind == DoodleOutcomeKind_Completed) {
        printf("stop: completed\n");
    } else if (oc->kind == DoodleOutcomeKind_Paused) {
        DoodlePosition pos;
        bool has = false;
        if (oc->pause_reason == DoodlePauseReason_RaiseTrap) {
            doodle_trapped_raise_position(inst, &pos, &has);
        } else {
            doodle_completed_position(inst, &pos, &has);
            if (!has) doodle_current_position(inst, &pos, &has);
        }
        printf("stop: paused %s @ ", reason_name(oc->pause_reason));
        put_position(inst, pos.module, has ? pos.span_start : 0);
        putchar('\n');
    } else if (oc->kind == DoodleOutcomeKind_Raised) {
        char kind[128];
        size_t klen = 0;
        doodle_raised_kind(inst, (uint8_t *)kind, sizeof kind, &klen);
        printf("stop: raised %.*s @ main#%u\n", (int)klen, kind, oc->has_span ? oc->span_start : 0);
    } else if (oc->kind == DoodleOutcomeKind_Faulted) {
        printf("stop: faulted %s\n", fault_name(oc->fault));
    } else {
        die("unexpected drive stop (suspended/import are not in the drive corpus)");
    }
}

/* Emits the live stack (`stack:`) after a stop, if any frame has a call site — innermost first,
 * `<name>@<module>#<offset>` with a `×<tail>` suffix for a proper-tail-call-reused frame. */
static void emit_stack(DoodleInstance *inst) {
    uint32_t count = 0, gen = 0;
    if (doodle_stack_frame_count(inst, &count, &gen) != DoodleStatus_Ok) return;
    int printed = 0;
    for (uint32_t i = 0; i < count; i++) {
        DoodleFrame f;
        if (doodle_frame_at(inst, gen, i, &f) != DoodleStatus_Ok || !f.has_call_site) continue;
        fputs(printed ? ", " : "stack: ", stdout);
        printed = 1;
        if (f.has_callable) {
            DoodleHandle h = DOODLE_NULL_HANDLE;
            char name[128];
            size_t nl = 0;
            bool has_name = false;
            if (doodle_frame_callable(inst, gen, i, &h) == DoodleStatus_Ok) {
                doodle_callable_name(inst, h, &has_name, (uint8_t *)name, sizeof name, &nl);
                if (has_name) printf("%.*s@", (int)nl, name);
                doodle_release(inst, h);
            }
        }
        put_position(inst, f.call_site.module, f.call_site.span_start);
        if (f.tail_count > 0) printf("\xc3\x97%llu", (unsigned long long)f.tail_count); /* × */
    }
    if (printed) putchar('\n');
}

/* Drives a `mode: run` fixture: interleaved output + capability I/O, then a terminal outcome. */
static void run_loop(DoodleInstance *inst, const char *modules_dir) {
    printf("transcript v1\nmode: run\n");
    uint8_t *obuf = malloc(4096);
    size_t ocap = 4096, olast = 0;
    DoodleOutcome oc;
    doodle_drive(inst, DoodleDirective_RunToCompletion, &oc);
    for (;;) {
        emit_output(inst, &obuf, &ocap, &olast);
        if (oc.kind == DoodleOutcomeKind_Completed) {
            printf("outcome: completed\n");
            break;
        } else if (oc.kind == DoodleOutcomeKind_Raised) {
            char kind[128];
            size_t klen = 0;
            doodle_raised_kind(inst, (uint8_t *)kind, sizeof kind, &klen);
            printf("outcome: raised %.*s @ main#%u\n", (int)klen, kind,
                   oc.has_span ? oc.span_start : 0);
            break;
        } else if (oc.kind == DoodleOutcomeKind_Faulted) {
            printf("outcome: faulted %s\n", fault_name(oc.fault));
            break;
        } else if (oc.kind == DoodleOutcomeKind_SuspendedImport) {
            resolve_import(inst, modules_dir, oc.request_count, &oc);
        } else if (oc.kind == DoodleOutcomeKind_Suspended) {
            const char *name = cap_name(oc.capability);
            if (!name) die("unknown capability id");
            DoodlePosition pos;
            bool has = false;
            doodle_current_position(inst, &pos, &has);
            printf("req: %s @ ", name);
            put_position(inst, pos.module, has ? pos.span_start : 0);
            putchar('\n');
            struct Input *in = next_input(name);
            if (!in) die("no scripted input for a capability request");
            printf("res: %s\n", in->response);
            if (strncmp(in->response, "raise ", 6) == 0) {
                doodle_resolve_raise(inst, make_value(inst, in->response + 6), &oc);
            } else {
                doodle_resolve(inst, make_value(inst, in->response), &oc);
            }
        } else {
            die("run mode produced a Paused outcome");
        }
    }
    free(obuf);
}

/* Drives a `mode: drive` fixture: apply setup, then each action's `step:`/`stop:`/`stack:`. */
static void drive_loop(DoodleInstance *inst) {
    printf("transcript v1\nmode: drive\n");
    drive_setup(inst);
    for (int i = 0; i < g_action_count; i++) {
        DoodleOutcome oc;
        drive_action(inst, g_actions[i], &oc);
        printf("step: %s\n", g_actions[i]);
        emit_stop(inst, &oc);
        emit_stack(inst);
    }
}

int main(int argc, char **argv) {
    if (argc < 2) die("usage: conformance <fixture.doodle> [<modules_dir>]");
    const char *modules_dir = argc >= 3 ? argv[2] : "";
    size_t src_len = 0;
    uint8_t *source = slurp_path(argv[1], &src_len);
    read_job();

    DoodleRegistry *reg = build_registry();
    DoodleInstance *inst = NULL;
    DoodleStatus status =
        doodle_load_with_registry(source, src_len, NULL, reg, &inst, NULL, 0, NULL);
    if (status != DoodleStatus_Ok || !inst) die("load failed (a run/drive fixture must load clean)");

    if (g_drive) {
        drive_loop(inst);
    } else {
        run_loop(inst, modules_dir);
    }

    doodle_free(inst);
    free(source);
    return 0;
}
