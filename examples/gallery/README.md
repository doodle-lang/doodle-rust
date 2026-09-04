# Doodle gallery

Small example programs you can run with the `doodle` CLI:

```sh
cargo run -p doodle-cli -- run examples/gallery/<program>.doodle
# or, once built:
doodle run examples/gallery/<program>.doodle
```

## Programs

| Program | Shows |
| --- | --- |
| `greeting.doodle` | `read_line` + `print` + string interpolation — a tiny interactive program. |
| `random.doodle` | `random`, and `--seed N` for a reproducible sequence. |
| `clock.doodle` | `time` — reading the real wall clock. |
| `hello_modules.doodle` | `import` — it uses its sibling module `sayings.doodle`. |
| `spiral.doodle` | Turtle graphics: `import turtle.*`, then `forward`/`right` inside a `repeat`. |

`turtle.doodle` and `sayings.doodle` are library modules the programs import, not
programs to run on their own.

## Two things worth knowing

**Drawing renders to text, and never silently.** A terminal can't show pixels, so a
turtle program's drawing is rendered as text: a quiet end-of-run summary by default
(`drew 40 line segments …`), or one line per drawing command with `--draw-log`:

```sh
doodle run examples/gallery/spiral.doodle --draw-log
# line (0,0) -> (0,8) #1e5ac8ff
# turtle (0,8) heading=92 shown
# ...
# drew 40 line segments spanning (…) to (…); open this program in the browser playground …
```

The picture itself renders in the browser playground — the terminal host is honest
that it drew something and points you there. The drawing is never discarded quietly.

**Real time and randomness enter only as capability resolutions — so runs replay.**
`time` reads the wall clock and `random` draws from an entropy-seeded stream, so they
differ each run. But they reach a program only through the engine's recordable
capability boundary, never from inside the engine, so a recorded run replays
bit-for-bit and `--seed N` makes `random` reproducible on the spot. The policy is a
CLI convenience; the deterministic architecture is intact underneath (E§11).
