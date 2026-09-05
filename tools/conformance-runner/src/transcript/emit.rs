//! The transcript **emitters** (M7.5d): run a `mode: run` / `mode: drive` fixture and record its
//! execution as a [`Transcript`]. They reuse the exact drive logic the matcher checks against —
//! `drive::drive_action` (so import auto-resolution matches), the `CapabilityQueues`, and the
//! outcome→stop translation — so a committed transcript can never disagree with what `expect-*`
//! matching asserts on the same run.

use super::{Event, Pos, StackElem, Stop, Terminal, Transcript};
use crate::capability::{capability_name, registry};
use crate::drive::{apply_setup, drive_action, fault_kind, paused_position, reason_name};
use crate::matcher::{CapabilityQueues, build_resolution, error_messages};
use crate::model::{Mode, Test};
use doodle_core::drive::{
    Directive, ImportResolution, Limits, Outcome, resolve as resolve_capability, resolve_import,
    run,
};
use doodle_core::machine::Instance;
use doodle_core::parse::parse_program;
use doodle_core::resolve::resolve;
use doodle_core::source::{LineIndex, normalize};
use doodle_core::span::ModuleId;
use std::path::Path;

/// Records a `mode: run` fixture (D-M7-20): its interleaved output + capability request/resolution,
/// then the terminal outcome. Imports resolve transparently (like the matcher) and are not recorded.
pub(crate) fn record_run(
    test: &Test,
    source: &str,
    modules_dir: Option<&Path>,
) -> Result<Transcript, Vec<String>> {
    let (mut instance, nfc) = load(source)?;
    let index = LineIndex::new(&nfc);
    let mut events = Vec::new();
    let mut queues = CapabilityQueues::new(&test.inputs);
    let mut last_out = 0usize;
    let mut outcome = run(&mut instance, Directive::RunToCompletion);
    let terminal = loop {
        // Coalesce the output emitted since the last event into one `out:` run.
        let cur = instance.output().len();
        if cur > last_out {
            let chunk = instance.output()[last_out..].to_vec();
            events.push(Event::Out(chunk));
            last_out = cur;
        }
        match &outcome {
            Outcome::SuspendedImport(req) => {
                let path: Vec<String> = req.path.iter().map(|s| s.to_string()).collect();
                outcome = resolve_import(&mut instance, import_resolution(&path, modules_dir)?);
            }
            Outcome::Suspended(req) => {
                let cap_id = req.capability.0;
                let args: Vec<_> = req.args.clone();
                let name = capability_name(cap_id)
                    .ok_or_else(|| vec![format!("unknown capability id {cap_id}")])?;
                let pos = position(&instance, &index, &nfc, current_offset(&instance));
                events.push(Event::Req {
                    capability: name.to_string(),
                    pos,
                });
                let response = queues
                    .next(name)
                    .ok_or_else(|| vec![format!("no scripted `input:` for capability `{name}`")])?;
                events.push(Event::Res(response.clone()));
                for handle in args {
                    let _ = instance.release(handle);
                }
                let resolution = build_resolution(&mut instance, &response).map_err(|e| vec![e])?;
                outcome = resolve_capability(&mut instance, resolution);
            }
            Outcome::Completed(_) => break Terminal::Completed,
            Outcome::Raised(value, trace) => {
                let (kind, _message) = instance.describe_raised(*value);
                let pos = position(&instance, &index, &nfc, trace.raised_at.map(|s| s.start));
                break Terminal::Raised { kind, pos };
            }
            Outcome::Faulted(fault) => {
                break Terminal::Faulted {
                    kind: fault_kind(*fault),
                };
            }
            Outcome::Paused(_) => {
                return Err(vec!["run mode produced a Paused outcome".to_string()]);
            }
        }
    };
    events.push(Event::Outcome(terminal));
    Ok(Transcript {
        mode: Mode::Run,
        events,
    })
}

/// Records a `mode: drive` fixture: each step's action, the stop it produced, and the live stack.
pub(crate) fn record_drive(
    test: &Test,
    source: &str,
    modules_dir: Option<&Path>,
) -> Result<Transcript, Vec<String>> {
    let script = test
        .drive
        .as_ref()
        .expect("a `mode: drive` test carries a parsed drive script");
    let (mut instance, nfc) = load(source)?;
    let index = LineIndex::new(&nfc);
    apply_setup(&mut instance, script);
    let mut events = Vec::new();
    for step in &script.steps {
        let outcome = drive_action(&mut instance, &step.action, &step.expect, modules_dir)
            .map_err(|e| vec![e])?;
        events.push(Event::Step(super::render_action(&step.action)));
        events.push(Event::Stop(outcome_to_stop(
            &instance, &outcome, &index, &nfc,
        )));
        let stack = record_stack(&mut instance, &index, &nfc);
        if !stack.is_empty() {
            events.push(Event::Stack(stack));
        }
    }
    Ok(Transcript {
        mode: Mode::Drive,
        events,
    })
}

/// Loads a fixture's program (a load error is a FAIL, not part of the transcript), returning the
/// instance + its NFC source. The line index is built by the caller (it borrows the source).
fn load(source: &str) -> Result<(Instance, String), Vec<String>> {
    let nfc = normalize(source).into_owned();
    let parsed = parse_program(&nfc, ModuleId(0));
    let errs = error_messages(&parsed.diagnostics);
    if !errs.is_empty() {
        return Err(errs);
    }
    let resolved = resolve(parsed.ast, parsed.root, ModuleId(0));
    let errs = error_messages(&resolved.diagnostics);
    if !errs.is_empty() {
        return Err(errs);
    }
    // Entry id `main` matches the matcher/drive runner (E§3.2) — the entry-relative module label.
    let instance = Instance::load(resolved.module, Limits::default(), registry(), "main");
    Ok((instance, nfc))
}

/// Translates an `Outcome` to a transcript [`Stop`] — the same translation `drive::check_step`
/// checks against, so a recorded stop matches an asserted one.
fn outcome_to_stop(instance: &Instance, outcome: &Outcome, index: &LineIndex, nfc: &str) -> Stop {
    match outcome {
        Outcome::Completed(_) => Stop::Completed,
        Outcome::Paused(reason) => Stop::Paused {
            reason: reason_name(*reason).to_string(),
            pos: position(instance, index, nfc, paused_position(instance, *reason)),
        },
        Outcome::Raised(value, trace) => {
            let (kind, _message) = instance.describe_raised(*value);
            Stop::Raised {
                kind,
                pos: position(instance, index, nfc, trace.raised_at.map(|s| s.start)),
            }
        }
        Outcome::Suspended(req) => Stop::Suspended {
            capability: capability_name(req.capability.0).unwrap_or("?").to_string(),
            pos: position(instance, index, nfc, current_offset(instance)),
        },
        Outcome::SuspendedImport(req) => Stop::Import {
            path: req.path.join("."),
            pos: position(instance, index, nfc, current_offset(instance)),
        },
        Outcome::Faulted(fault) => Stop::Faulted {
            kind: fault_kind(*fault),
        },
    }
}

/// The live stack at a stop (innermost first), only the frames with a call site (the module top and
/// a native-invoked block have none) — the drive-script stack shape (E§8.2/§8.3).
fn record_stack(instance: &mut Instance, index: &LineIndex, nfc: &str) -> Vec<StackElem> {
    let frames = instance.stack_walk();
    frames
        .iter()
        .filter_map(|frame| {
            let line = index.position_at(nfc, frame.call_site?.start).line;
            let name = frame
                .callable
                .and_then(|h| instance.callable_name(h).ok().flatten());
            Some(StackElem {
                name,
                line,
                tail: frame.tail_count,
            })
        })
        .collect()
}

/// The byte offset of the instance's current position, if it has an active frame.
fn current_offset(instance: &Instance) -> Option<u32> {
    instance.current_position().map(|p| p.span.start)
}

/// Builds a transcript [`Pos`] from an offset (mapped through `index`/`nfc`) and the instance's
/// current **entry-relative** module (the entry is `main`; an import is its canonical id). A
/// terminal instance (no active frame) defaults to the entry — every current corpus raise is there.
fn position(instance: &Instance, index: &LineIndex, nfc: &str, offset: Option<u32>) -> Pos {
    let module = instance
        .current_position()
        .and_then(|p| instance.module_canonical_id(p.module).map(str::to_string))
        .unwrap_or_else(|| "main".to_string());
    let (line, column) = offset.map_or((0, 0), |o| {
        let p = index.position_at(nfc, o);
        (p.line, p.column)
    });
    Pos {
        module,
        line,
        column,
    }
}

/// Resolves an import to a sibling module file (`<modules_dir>/<path>.doodle`), else `NotFound` —
/// the same transparent resolution the matcher does.
fn import_resolution(
    path: &[String],
    modules_dir: Option<&Path>,
) -> Result<ImportResolution, Vec<String>> {
    let joined = path.join("/");
    let file = modules_dir.map(|d| d.join(&joined).with_extension("doodle"));
    Ok(match file {
        Some(p) if p.is_file() => match std::fs::read_to_string(&p) {
            Ok(text) => ImportResolution::Source {
                text,
                canonical_id: joined,
            },
            Err(e) => return Err(vec![format!("reading module {}: {e}", p.display())]),
        },
        _ => ImportResolution::NotFound,
    })
}
