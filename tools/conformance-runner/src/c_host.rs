//! The C-host conformance orchestrator (M7.5e): drive each `mode: run` / `mode: drive` fixture
//! through the example C host (`examples/c-host/conformance.c`) and compare its transcript to the
//! canonical sidecar — the third surface, transitively identical to native/wasm via the oracle (M7.5d).
//!
//! The C host emits positions as `<canonical-module>#<byte-offset>` (the ABI hands out byte spans,
//! and there is no NFC-source accessor). This module **finalizes** them to line:col with doodle-core's
//! own `normalize` + `LineIndex` — the identical mapping the native emitter used to generate the
//! canonical file — then string-compares. A `stack:` element's call site finalizes to just its line.

use crate::model::{Mode, Test};
use crate::transcript::{render_action, render_response};
use doodle_core::source::{LineIndex, Position, normalize};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Runs one `mode: run` / `mode: drive` fixture through the C host and returns its **finalized**
/// transcript (offsets mapped to line:col), or a runner-level error (spawn/IO failure, a non-UTF-8
/// or malformed C output).
pub(crate) fn transcript_through_c(
    c_host: &Path,
    test: &Test,
    entry: &Path,
    modules_dir: Option<&Path>,
) -> Result<String, String> {
    let job = match test.mode {
        Mode::Run => run_job(test),
        Mode::Drive => drive_job(test),
        Mode::Static => return Err("a static fixture has no transcript".to_string()),
    };
    let raw = spawn(c_host, entry, modules_dir, &job)?;
    finalize(&raw, entry, modules_dir)
}

/// The C-host job for a `mode: run` fixture: the scripted `input:` responses, in canonical form (so
/// the host's echoed `res:` matches).
fn run_job(test: &Test) -> String {
    let mut job = String::from("mode run\n");
    for input in &test.inputs {
        job.push_str(&format!(
            "input {} {}\n",
            input.capability,
            render_response(&input.response)
        ));
    }
    job
}

/// The C-host job for a `mode: drive` fixture: the debug setup, then each action's `step:` label.
fn drive_job(test: &Test) -> String {
    let script = test
        .drive
        .as_ref()
        .expect("a drive test carries a parsed script");
    let mut job = String::from("mode drive\n");
    for (canonical, line) in &script.breakpoints {
        job.push_str(&format!("break {canonical} {line}\n"));
    }
    if script.raise_trap {
        job.push_str("raise-trap on\n");
    }
    if script.subexpr {
        job.push_str("obs subexpr\n");
    }
    for step in &script.steps {
        job.push_str(&format!("do {}\n", render_action(&step.action)));
    }
    job
}

/// Spawns the C host `conformance <entry> [<modules_dir>]`, feeds `job` on stdin, and returns its
/// captured stdout. A non-zero exit or stderr output is a runner error (a mis-driven fixture).
fn spawn(
    c_host: &Path,
    entry: &Path,
    modules_dir: Option<&Path>,
    job: &str,
) -> Result<String, String> {
    let mut child = Command::new(c_host)
        .arg(entry)
        .arg(modules_dir.map(Path::as_os_str).unwrap_or_default())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawning the C host: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("no stdin pipe to the C host")?
        .write_all(job.as_bytes())
        .map_err(|e| format!("writing the job to the C host: {e}"))?;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("waiting on the C host: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "the C host exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|_| "the C host emitted non-UTF-8".to_string())
}

/// Rewrites every `<module>#<offset>` position in the C host's transcript to line:col using the
/// module's NFC source — the same mapping (`LineIndex::position_at`) the native emitter used, so a
/// correct C run finalizes byte-for-byte to the canonical file. `req:`/`outcome:`/`stop:` positions
/// become `<module>:<line>:<col>`; a `stack:` elem's call site becomes just its line (the drive
/// stack encoding). Positions never appear in `out:`/`res:` payloads, so those pass through.
fn finalize(raw: &str, entry: &Path, modules_dir: Option<&Path>) -> Result<String, String> {
    // A per-module NFC source cache (canonical id -> normalized source). Runner-internal, not a
    // Doodle-observable path, but kept a Vec to avoid any default-hasher habit (CLAUDE.md).
    let mut sources: Vec<(String, String)> = Vec::new();
    let mut out = String::new();
    for line in raw.lines() {
        let rewritten = if let Some(payload) = line.strip_prefix("stack: ") {
            finalize_stack(payload, entry, modules_dir, &mut sources)?
        } else if is_position_line(line) {
            finalize_position(line, entry, modules_dir, &mut sources)?
        } else {
            line.to_string()
        };
        out.push_str(&rewritten);
        out.push('\n');
    }
    Ok(out)
}

/// Whether `line` carries a `<module>#<offset>` position (a stop/request/outcome with ` @ `).
fn is_position_line(line: &str) -> bool {
    (line.starts_with("req: ") || line.starts_with("outcome: ") || line.starts_with("stop: "))
        && line.contains(" @ ")
}

/// Rewrites the `<module>#<offset>` after ` @ ` on a position line to `<module>:<line>:<col>`.
fn finalize_position(
    line: &str,
    entry: &Path,
    modules_dir: Option<&Path>,
    sources: &mut Vec<(String, String)>,
) -> Result<String, String> {
    let at = line.find(" @ ").expect("is_position_line guaranteed ` @ `");
    let (head, tail) = line.split_at(at + 3);
    let (module, offset) = split_offset(tail)?;
    let pos = position(module, offset, entry, modules_dir, sources)?;
    Ok(format!("{head}{module}:{}:{}", pos.line, pos.column))
}

/// Rewrites each `stack:` element's `<name>@<module>#<offset>[×<tail>]` call site to
/// `<name>@<line>[×<tail>]` — the drive-script stack encoding (line only, no module/column).
fn finalize_stack(
    payload: &str,
    entry: &Path,
    modules_dir: Option<&Path>,
    sources: &mut Vec<(String, String)>,
) -> Result<String, String> {
    let mut elems = Vec::new();
    for elem in payload.split(", ") {
        let hash = elem
            .find('#')
            .ok_or_else(|| format!("stack element lacks `#`: {elem:?}"))?;
        // The left of `#` is `<name>@<module>` or a bare `<module>` (a nameless block frame).
        let (name_at, module) = match elem[..hash].rfind('@') {
            Some(a) => (&elem[..=a], &elem[a + 1..hash]),
            None => ("", &elem[..hash]),
        };
        // The right of `#` is the offset digits then an optional `×<tail>` suffix.
        let right = &elem[hash + 1..];
        let end = right
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(right.len());
        let offset: u32 = right[..end]
            .parse()
            .map_err(|_| format!("bad stack offset: {elem:?}"))?;
        let pos = position(module, offset, entry, modules_dir, sources)?;
        elems.push(format!("{name_at}{}{}", pos.line, &right[end..]));
    }
    Ok(format!("stack: {}", elems.join(", ")))
}

/// Splits a `<module>#<offset>` token into its parts.
fn split_offset(token: &str) -> Result<(&str, u32), String> {
    let hash = token
        .find('#')
        .ok_or_else(|| format!("C host position lacks `#`: {token:?}"))?;
    let offset = token[hash + 1..]
        .parse()
        .map_err(|_| format!("bad C host offset: {token:?}"))?;
    Ok((&token[..hash], offset))
}

/// Maps a `(module, byte-offset)` to a 1-based [`Position`] through the module's NFC source.
fn position(
    module: &str,
    offset: u32,
    entry: &Path,
    modules_dir: Option<&Path>,
    sources: &mut Vec<(String, String)>,
) -> Result<Position, String> {
    let nfc = module_source(module, entry, modules_dir, sources)?;
    Ok(LineIndex::new(nfc).position_at(nfc, offset))
}

/// The NFC source of module `canonical` (cached): the entry file for `main`, else the sibling
/// `<modules_dir>/<canonical>.doodle` (a dotted path maps `/`-wise).
fn module_source<'a>(
    canonical: &str,
    entry: &Path,
    modules_dir: Option<&Path>,
    cache: &'a mut Vec<(String, String)>,
) -> Result<&'a str, String> {
    if let Some(i) = cache.iter().position(|(name, _)| name == canonical) {
        return Ok(&cache[i].1);
    }
    let path = if canonical == "main" {
        entry.to_path_buf()
    } else {
        let dir = modules_dir.ok_or_else(|| format!("no modules dir for import `{canonical}`"))?;
        dir.join(canonical.replace('.', "/"))
            .with_extension("doodle")
    };
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("reading module {} for finalization: {e}", path.display()))?;
    cache.push((canonical.to_string(), normalize(&raw).into_owned()));
    Ok(&cache.last().unwrap().1)
}
