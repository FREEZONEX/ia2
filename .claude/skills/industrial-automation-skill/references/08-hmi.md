# HMI — generating operator screens

A screen is one JSON document under the project's `hmi/` directory,
addressed by slug like a POU (`cs get hmi/overview`). You author it the way
you author everything else in IA2: through the CLI against the running
server, with the IDE reflecting every change live. The intended rhythm is
**generate a baseline, then reshape it element by element** — each `cs hmi
op` batch renders immediately in any open canvas with a brief spawn
animation on exactly the nodes you touched, so the human watches the screen
assemble as you work. Prefer many small op batches over one big save: the
live feedback is the point, and atomic batches keep every intermediate
state valid.

## The workflow

Start from project truth, not from a blank page. `cs hmi generate overview`
builds a deterministic first pass — alarm bar on top, one section per POU
file, BOOLs as indicators, numerics as value readouts, `*_sp`-named
numerics as setpoint inputs with confirmed write actions, and a trend over
the first numerics. It is deliberately boring and always the same for the
same project; your job is the creative pass on top of it. If the screen
already exists, generate returns 409 unless you pass `--force` — never
force over a screen a human may have curated without asking.

Then look before you edit. `cs get hmi/overview` prints the document;
`cs get hmi-symbols` prints the palette contract (each built-in symbol's
bindable keys, props and default size); `cs check hmi/overview` validates
structure and warns about bindings that name variables no POU declares.
With the picture in hand, reshape incrementally:

```bash
# move the trend, retitle the screen
echo '[{"op":"update_node","id":"trend_main","patch":{"x":24,"y":520,"w":1232}},
      {"op":"set_meta","title":"Carbonation — Overview"}]' | cs hmi op overview --from -

# add a tank wired to a level variable
echo '[{"op":"add_node","node":{"id":"tank_t201","type":"symbol","symbol":"tank",
       "x":80,"y":90,"w":140,"h":200,
       "props":{"label":"T-201","unit":"%"},
       "bind":{"value":"level_pct","alarm":"level_hh"}}}]' | cs hmi op overview --from -

# a valve the operator can command, with confirmation
echo '[{"op":"add_node","node":{"id":"valve_yv201","type":"symbol","symbol":"valve",
       "x":260,"y":250,"w":48,"h":48,"props":{"label":"YV-201"},
       "bind":{"open":"yv201_fb"},
       "action":{"tap":{"kind":"toggle","variable":"yv201_cmd","confirm":true}}}}]' \
  | cs hmi op overview --from -
```

The three op kinds beyond `add_node`: `update_node` shallow-merges a patch
(object fields like `bind`/`action`/`props` merge one level, scalars
replace, `null` deletes a key; `id` and `type` are immutable — remove and
re-add to change a node's type), `remove_node` deletes a node and its
subtree, `set_meta` edits title/level/grid. A batch is atomic: one bad op
rejects the whole batch with a message naming the op index, and the
document on disk is untouched.

Close the loop visually. After a few batches, render the screen and look
at it the way the Pencil workflow looks at a canvas: open
`http://127.0.0.1:3001/?project=NAME&hmi=overview` (or the vite dev port)
headless, screenshot, and judge your own layout — overlaps, crowding,
reading order, whether color is carrying state or decoration. Iterate with
more ops. When the program is running, bindings go live and the screenshot
shows real values, which is the strongest self-review available.

## The document model, briefly

Nodes are a closed set: `group` (absolute positioning only — flow
layouts are retired; lay children out with coordinates), `text`,
`value`, `symbol`, `trend`, `alarmbar`, `alarmlist`
(`{"type":"alarmlist","max_rows":N}` — an alarm-summary table with
per-row ack, the multi-row companion to the single-line `alarmbar`),
`button`, `input`, `nav`, `shape` (`rect` / `ellipse` / `line` / `polyline`).
Coordinates live on a fixed grid (default 1280×800, snap 8) that every
client letterboxes identically. `bind` maps a prop to a variable — a bare
name in the common case, or `{"variable":"x_raw","expr":"x / 100",
"format":"%.1f"}` for scaling; names resolve exactly like the Monitor's,
including `instance.variable` on multi-PROGRAM runs. When the same bare
name exists in more than one PROGRAM the renderer shows "—" rather than
guessing, and `check` tells you to qualify it. Expressions see only
the single bound value `x` on purpose — logic that spans variables belongs
in a POU, not hidden in a screen. `action` is the only write path
(`write` / `toggle` / `pulse` / `set_value` / `nav`), every action is
declared in the reviewable document, and `confirm` defaults to true —
leave it on for anything that moves the plant. A pulse's 0-write is a
runtime-side guarantee (the request carries `pulse_ms`), so it survives
the operator's tab; keep `ms` under 10 s or check will tell you it's
really a toggle. Buttons also honour an optional `bind.on` — the button
lights while the bound value is truthy, so a toggle shows the state it
controls without a separate indicator. `bind.enable` gates a button or
input: 0 (or no live data) disables both the visual and the gesture —
hide Start while running with `{"variable":"running","expr":"!x"}`. The
`increment` action kind steps a setpoint from its LIVE value
(`{"kind":"increment","variable":"speed_sp","step":5,"min":0,"max":100}`)
— the widget's bounds are the safety envelope, and it refuses when no
live base exists. For a state toggle that should read as a switch rather
than a momentary button, use the `switch` symbol (rocker + ON/OFF
captions) with an `action.tap` toggle. `generate` also groups mapped
variables per device (an equipment section per wired device, qualified
`instance.variable` bindings) — regenerate after wiring changes to see
the plant's shape, then curate.

## Maps: values become colors and words, declaratively

A binding spec can carry `map` — an ordered rule list applied after
`expr`, first match wins: `{"eq":1,"out":"RUNNING"}` matches exactly,
`{"min":80,"out":"alarm"}` and `{"min":50,"max":80,"out":"warn"}` match
half-open `[min,max)` ranges, and an entry with no condition is the
catch-all (put it last). The matched `out` string is the binding's output,
and it is the only way a value becomes a color or a word — there is no
scripting. Generic bind keys the renderer honors on top of each node's
own: `visible` on every node (0 hides the element), `color` on
`text`/`value`/`tank`/`bar`/`led`/`pipe`, `text` on text nodes, and
`fill`/`stroke` on shapes. Color outputs should speak tokens — `ok`,
`warn`, `alarm`, `info`, `muted`, `fg`, `agent` — which track the design
system; any other string passes through as a literal CSS color when a
brand truly needs it. `"format":"%s"` shows the raw value text, which is
how STRING variables reach a screen.

```bash
# temperature readout that turns amber above 50 and red above 80
echo '[{"op":"update_node","id":"temp_val","patch":{"bind":{"color":{
  "variable":"temp_c","map":[
    {"min":80,"out":"alarm"},{"min":50,"out":"warn"},{"out":"ok"}]}}}}]' \
  | cs hmi op overview --from -

# a state word instead of a number
echo '[{"op":"add_node","node":{"id":"phase_lbl","type":"text","text":"—",
  "x":80,"y":120,"style":"title",
  "bind":{"text":{"variable":"phase","map":[
    {"eq":0,"out":"IDLE"},{"eq":1,"out":"FILLING"},{"eq":2,"out":"CARBONATING"},
    {"out":"?"}]}}}}]' | cs hmi op overview --from -
```

## Style: high-performance HMI, already built in

The built-in symbols implement ISA-101 on IA2's design tokens, so a
generated screen is compliant by construction: calm gray-on-warm-neutral
in the normal state, color only when it means something (running/healthy
green, attention ochre, fault red). Preserve that when you compose —
resist decorating. One alarm bar per screen at the top; group by process
area, reading left-to-right in flow order; use `level` honestly (1 plant
overview → 4 diagnostic detail) and `nav` nodes to descend, rather than
cramming levels together.

Seventeen symbols (see `cs get hmi-symbols` for each one's contract). Beyond the
original nine, reach for: `analog` — the moving analog indicator ISA-101
prefers over gauges (scale + shaded normal band via `lo`/`hi` props + live
pointer + `sp` bind for the setpoint tick); `bar` — linear fill, `h`/`v`;
`led` — headline numeric on a dark plate for the one number the room reads
from across the floor; `sparkline` — axis-less inline history; `pipe` —
process line whose `flow` bind animates travel (sign flips direction, zero
is still); `fan` and `conveyor` — spin/travel while running. Text nodes
take `color`/`size`/`align`/`weight` props; shapes take
`fill`/`stroke`/`stroke_width`/`rx`/`dash`. Motion is state, not
decoration: running equipment spins, flowing lines travel, levels ease —
and all of it is also legible from color alone (reduced-motion clients
drop the animation, not the meaning).

## The screens travel with the project — the edge serves them

`cs deploy <edge>` ships the whole project directory, `hmi/` included —
plus the built web assets when the IDE server has them — so whatever
screens exist at deploy time are exactly what the edge box has. The
standard systemd unit starts the runtime with `--static-dir`, which
serves a standalone operator panel on the runtime's port: `/hmi` lists
the deployed screens and `/hmi/<screen>` renders one live against that
runtime's own `/events` and `/write` — same canvas, same confirm flows
as the IDE, no IDE required. The default bind is loopback (the ssh-tunnel
trust perimeter), so reach it through the tunnel `cs api POST
/api/edges/<edge>/attach` opens (`detach` to close), or widen
`--bind` in the unit as a deliberate ops decision when operator tablets
need direct access. The panel is read-only as a document (no arrange, no
ops); to change a screen, edit it in the project and redeploy.

## Visual validation in the IDE

Use **Fit** to inspect the entire authored screen, then **100%** to check text and
controls at their authored size. Both axes must fit without a stray scrollbar;
100% may scroll. **Operate** permits configured writes, while **Arrange** only
edits layout. Switching modes clears pending write confirmations. An unsuccessful
write, layout save or alarm acknowledgment stays visibly failed.

Alarm lists show active or unacknowledged alarms by default. **Show all** includes
cleared and never-raised definitions; a never-raised timestamp is a dash. Check
both themes, the smallest intended window, trend bounds, and real changing values
before calling an HMI visually validated. Software simulation is not fieldbus proof.
