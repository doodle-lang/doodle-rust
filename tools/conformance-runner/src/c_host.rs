//! The C-host conformance orchestrator (M7.5e): drive each `mode: run` fixture through the example
//! C host (`examples/c-host/conformance.c`) and compare its transcript to the canonical sidecar —
//! the third surface, transitively identical to native/wasm via the shared oracle (M7.5d).
//!
//! The C host emits positions as `<canonical-module>#<byte-offset>` (the ABI hands out byte spans,
//! and there is no NFC-source accessor). This module **finalizes** them to `<module>:<line>:<col>`
//! with doodle-core's own `normalize` + `LineIndex` — the identical mapping the native emitter used
//! to generate the canonical file — then string-compares. Drive fixtures are the M7.5e-2 extension.

use crate::model::Test;
use crate::transcript::render_response;
use doodle_core::source::{LineIndex, normalize};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Runs one `mode: run` fixture through the C host and returns its **finalized** transcript (offsets
/// mapped to line:col), or a runner-level error (spawn/IO failure, a non-UTF-8 or malformed C output).
pub(crate) fn transcript_through_c(
    c_host: &Path,
    test: &Test,
    entry: &Path,
    modules_dir: Option<&Path>,
) -> Result<String, String> {
    let mut job = String::from("mode run\n");
    for input in &test.inputs {
        job.push_str(&format!(
            "input {} {}\n",
            input.capability,
            render_response(&input.response)
        ));
    }
    let raw = spawn(c_host, entry, modules_dir, &job)?;
    finalize(&raw, entry, modules_dir)
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

/// Rewrites every `<module>#<offset>` position in the C host's transcript to `<module>:<line>:<col>`
/// using the module's NFC source — the same mapping (`LineIndex::position_at`) the native emitter
/// used, so a correct C run finalizes byte-for-byte to the canonical file.
fn finalize(raw: &str, entry: &Path, modules_dir: Option<&Path>) -> Result<String, String> {
    // A per-module NFC source cache (canonical id -> normalized source). Runner-internal, not a
    // Doodle-observable path, but kept a Vec to avoid any default-hasher habit (CLAUDE.md).
    let mut sources: Vec<(String, String)> = Vec::new();
    let mut out = String::new();
    for line in raw.lines() {
        // Positions only appear after ` @ ` on stop/request/outcome lines; never in `out:`/`res:`
        // payloads, so restrict the rewrite to those prefixes to avoid a false match on output.
        let positional = line.starts_with("req: ")
            || line.starts_with("outcome: ")
            || line.starts_with("stop: ");
        if positional && let Some(at) = line.find(" @ ") {
            let (head, tail) = line.split_at(at + 3);
            let hash = tail
                .find('#')
                .ok_or_else(|| format!("C host position lacks `#`: {line:?}"))?;
            let module = &tail[..hash];
            let offset: u32 = tail[hash + 1..]
                .parse()
                .map_err(|_| format!("bad C host offset: {line:?}"))?;
            let nfc = module_source(module, entry, modules_dir, &mut sources)?;
            let pos = LineIndex::new(nfc).position_at(nfc, offset);
            out.push_str(&format!("{head}{module}:{}:{}", pos.line, pos.column));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    Ok(out)
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
